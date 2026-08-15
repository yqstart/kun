//! SSH 主机密钥校验（TOFU：首次信任）与 known_hosts 持久化。
//!
//! OpenSSH known_hosts 语义的简化版：每个 `主机:端口` 保存首次连接时的
//! 服务器公钥 SHA256 指纹（格式同 `ssh-keygen -lf`），后续连接比对，
//! 不匹配则拒绝——防止中间人攻击（此前无条件接受所有服务器密钥）。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use russh::client;
use russh::keys::{HashAlg, PublicKey};

/// known_hosts 条目。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct KnownHostEntry {
    pub host: String,
    pub port: u16,
    /// SHA256 指纹（`SHA256:base64`，OpenSSH 格式）。
    pub fingerprint: String,
}

/// 全部已知主机。
#[derive(Debug, Default, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct KnownHosts {
    pub hosts: Vec<KnownHostEntry>,
}

/// 默认 known_hosts 文件路径：`~/.config/kun/known_hosts.toml`。
///
/// 环境变量 `KUN_KNOWN_HOSTS` 可覆盖（集成测试用：指向与测试 sshd
/// hostkey 同目录的文件——hostkey 随 /tmp 清理重建时指纹记录一并消失，
/// 不会因旧指纹不匹配导致测试失败）。
pub fn default_known_hosts_path() -> PathBuf {
    std::env::var("KUN_KNOWN_HOSTS")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(home)
                .join(".config")
                .join("kun")
                .join("known_hosts.toml")
        })
}

/// 进程级互斥：读-改-写 known_hosts 必须串行（终端与 SFTP 双连接并发首连时
/// 各自读文件会互相覆盖——丢失更新）。
static KNOWN_HOSTS_LOCK: Mutex<()> = Mutex::new(());

/// 加载 known_hosts；文件不存在或损坏时视为空（首次连接场景）。
pub(crate) fn load_known_hosts(path: &Path) -> KnownHosts {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|c| toml::from_str(&c).ok())
        .unwrap_or_default()
}

/// 保存 known_hosts（文件权限 0600：指纹防本地其他用户读取篡改）。
pub(crate) fn save_known_hosts(path: &Path, known: &KnownHosts) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let content = toml::to_string_pretty(known)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(path, content)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// 服务器主机密钥验证器（russh client Handler）。
///
/// TOFU：首次见到某 `主机:端口` 的密钥时接受并记录指纹；此后必须一致，
/// 不一致（服务器换密钥或中间人攻击）则拒绝，拒绝原因写入 `error` 供
/// 外层 connect 失败后包装成友好错误。
pub struct HostKeyVerifier {
    host: String,
    port: u16,
    save_path: PathBuf,
    /// 验证失败原因（外层连接失败后 take 读取）。
    pub(crate) error: Arc<Mutex<Option<String>>>,
}

impl HostKeyVerifier {
    /// 构造验证器（`error` 供外层持有引用，connect 失败时读原因）。
    pub fn new(host: String, port: u16, save_path: PathBuf) -> (Self, Arc<Mutex<Option<String>>>) {
        let error = Arc::new(Mutex::new(None));
        (
            Self {
                host,
                port,
                save_path,
                error: error.clone(),
            },
            error,
        )
    }

    /// 校验密钥：已知且一致 → 接受；首次 → 记录并接受；不一致 → 拒绝。
    fn verify(&self, key: &PublicKey) -> Result<bool, String> {
        let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
        let _guard = KNOWN_HOSTS_LOCK.lock().unwrap();
        let mut known = load_known_hosts(&self.save_path);
        match known
            .hosts
            .iter()
            .find(|e| e.host == self.host && e.port == self.port)
        {
            Some(entry) if entry.fingerprint == fingerprint => Ok(true),
            Some(entry) => Err(format!(
                "主机密钥指纹不匹配！\n服务器：{}:{}\n预期指纹：{}\n实际指纹：{}\n\
                 服务器可能已被替换（中间人攻击）。若确认是服务器重装系统，\
                 请删除 ~/.config/kun/known_hosts.toml 中的对应条目后重试。",
                self.host, self.port, entry.fingerprint, fingerprint
            )),
            None => {
                known.hosts.push(KnownHostEntry {
                    host: self.host.clone(),
                    port: self.port,
                    fingerprint,
                });
                if let Err(e) = save_known_hosts(&self.save_path, &known) {
                    log::warn!("保存 known_hosts 失败：{e}");
                }
                Ok(true)
            }
        }
    }
}

impl client::Handler for HostKeyVerifier {
    type Error = russh::Error;

    async fn check_server_key(&mut self, key: &PublicKey) -> Result<bool, Self::Error> {
        match self.verify(key) {
            Ok(ok) => Ok(ok),
            Err(msg) => {
                *self.error.lock().unwrap() = Some(msg);
                Ok(false)
            }
        }
    }
}

/// 去重检查：known_hosts 加载后同一 `主机:端口` 不应出现重复条目。
#[cfg(test)]
fn is_duplicate_free(known: &KnownHosts) -> bool {
    use std::collections::HashSet;
    let mut seen = HashSet::new();
    known
        .hosts
        .iter()
        .all(|e| seen.insert((e.host.clone(), e.port)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个测试独立临时文件（含测试名）——三个测试并行运行时不能共用
    /// 同一路径（会互相覆盖，曾导致全量测试偶发失败）。
    fn tmp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "kun-known-hosts-test-{}-{name}.toml",
            std::process::id()
        ))
    }

    /// 在当前线程 runtime 上阻塞执行校验（测试环境用）。
    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("创建 runtime 失败")
            .block_on(future)
    }

    /// TOFU 流程：首次接受并持久化，二次连接指纹一致接受、不一致拒绝。
    #[test]
    fn 首次信任与后续比对() {
        let path = tmp_path("tofu");
        let _ = std::fs::remove_file(&path);

        // 生成临时 ed25519 密钥对，用公钥模拟服务器密钥。
        let keypair = russh::keys::PrivateKey::random(
            &mut rand::rng(),
            russh::keys::ssh_key::Algorithm::Ed25519,
        )
        .expect("生成密钥失败");
        let public = keypair.public_key().clone();

        // 首次：接受并记录。
        let (v1, err1) = HostKeyVerifier::new("test.example.com".into(), 22, path.clone());
        let mut v1 = v1;
        let public1 = public.clone();
        let ok = block_on(async move {
            <HostKeyVerifier as client::Handler>::check_server_key(&mut v1, &public1).await
        })
        .expect("校验失败");
        assert!(ok, "首次连接应接受");
        assert!(err1.lock().unwrap().is_none());
        let known = load_known_hosts(&path);
        assert_eq!(known.hosts.len(), 1, "首次连接应记录指纹");
        assert!(is_duplicate_free(&known));

        // 第二次同密钥：接受，且不重复记录。
        let (v2, err2) = HostKeyVerifier::new("test.example.com".into(), 22, path.clone());
        let mut v2 = v2;
        let public2 = public.clone();
        let ok = block_on(async move {
            <HostKeyVerifier as client::Handler>::check_server_key(&mut v2, &public2).await
        })
        .expect("校验失败");
        assert!(ok, "指纹一致应接受");
        assert!(err2.lock().unwrap().is_none());
        assert_eq!(load_known_hosts(&path).hosts.len(), 1, "不应重复记录");

        // 第三次不同密钥：拒绝并给出原因。
        let other = russh::keys::PrivateKey::random(
            &mut rand::rng(),
            russh::keys::ssh_key::Algorithm::Ed25519,
        )
        .expect("生成密钥失败");
        let (v3, err3) = HostKeyVerifier::new("test.example.com".into(), 22, path.clone());
        let mut v3 = v3;
        let ok = block_on(async move {
            <HostKeyVerifier as client::Handler>::check_server_key(&mut v3, other.public_key())
                .await
        })
        .expect("校验失败");
        assert!(!ok, "指纹不一致应拒绝");
        assert!(
            err3.lock()
                .unwrap()
                .as_deref()
                .unwrap_or("")
                .contains("不匹配"),
            "应记录拒绝原因"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// 不同端口视为不同条目（同一主机可同时监听 22 与 2222）。
    #[test]
    fn 端口区分条目() {
        let path = tmp_path("ports");
        let _ = std::fs::remove_file(&path);
        let known = KnownHosts {
            hosts: vec![
                KnownHostEntry {
                    host: "h".into(),
                    port: 22,
                    fingerprint: "SHA256:a".into(),
                },
                KnownHostEntry {
                    host: "h".into(),
                    port: 2222,
                    fingerprint: "SHA256:b".into(),
                },
            ],
        };
        save_known_hosts(&path, &known).expect("保存失败");
        let loaded = load_known_hosts(&path);
        assert_eq!(loaded, known);
        let _ = std::fs::remove_file(&path);
    }

    /// 文件损坏时视为空（不影响连接，重新走首次信任）。
    #[test]
    fn 损坏文件视为空() {
        let path = tmp_path("corrupt");
        std::fs::write(&path, "not valid toml {{{").ok();
        assert!(load_known_hosts(&path).hosts.is_empty());
        let _ = std::fs::remove_file(&path);
    }
}
