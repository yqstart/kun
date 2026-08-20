//! 主机配置模型与持久化。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// 认证方式。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum Auth {
    /// 密码认证。
    Password(String),
    /// 私钥认证（路径 + 可选口令）。
    Key {
        path: PathBuf,
        passphrase: Option<String>,
    },
}

/// 主机配置。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostProfile {
    /// 显示名称。
    pub name: String,
    /// 主机地址。
    pub host: String,
    /// 端口（默认 22）。
    pub port: u16,
    /// 用户名。
    pub user: String,
    /// 认证方式。
    pub auth: Auth,
}

impl Default for HostProfile {
    fn default() -> Self {
        Self {
            name: String::new(),
            host: String::new(),
            port: 22,
            user: String::new(),
            auth: Auth::Password(String::new()),
        }
    }
}

/// 全部主机配置。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HostConfig {
    pub hosts: Vec<HostProfile>,
}

impl HostConfig {
    /// 加载配置文件；不存在时返回空配置。
    pub fn load(path: &std::path::Path) -> std::io::Result<HostConfig> {
        let content = std::fs::read_to_string(path)?;
        toml::from_str(&content)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    /// 保存配置到文件（unix 下强制 0600：配置含明文密码/私钥口令，
    /// 默认 umask 022 会产生 0644，本机其他用户可读）。
    ///
    /// 原子写入：先写临时文件再 rename 覆盖——进程被杀/磁盘满时不会把
    /// hosts.toml 截断或清空（曾因直接 write 出现主机列表丢失）。
    pub fn save(&self, path: &std::path::Path) -> std::io::Result<()> {
        let content = toml::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, content)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

/// 默认配置文件路径：`~/.config/mino/hosts.toml`。
pub fn default_config_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home)
        .join(".config")
        .join("mino")
        .join("hosts.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> HostConfig {
        HostConfig {
            hosts: vec![HostProfile {
                name: "测试服务器".into(),
                host: "example.com".into(),
                port: 22,
                user: "root".into(),
                auth: Auth::Key {
                    path: PathBuf::from("~/.ssh/id_ed25519"),
                    passphrase: None,
                },
            }],
        }
    }

    #[test]
    fn 配置序列化与反序列化() {
        let config = sample();
        let toml_str = toml::to_string_pretty(&config).unwrap();
        let parsed: HostConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed, config);
    }
}
