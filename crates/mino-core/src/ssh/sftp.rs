//! SFTP 客户端：后台线程驱动，UI 通过命令队列操作、事件流接收结果。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use russh_sftp::client::SftpSession;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc::{self, Receiver, Sender, UnboundedReceiver, UnboundedSender};

use crate::config::HostProfile;
use crate::ssh::{connect_verified, ssh_config};

/// 传输临时文件序号。临时文件与目标文件位于同一目录，成功后用 rename
/// 原子替换目标，避免失败传输破坏已有文件。
static PARTIAL_COUNTER: AtomicU64 = AtomicU64::new(0);

fn partial_suffix() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        PARTIAL_COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

fn partial_remote_path(remote: &str) -> String {
    if remote.is_empty() {
        format!(".mino-partial-{}", partial_suffix())
    } else {
        format!("{remote}.mino-partial-{}", partial_suffix())
    }
}

fn partial_local_path(local: &Path) -> PathBuf {
    let name = local
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("download");
    local
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!(".{name}.mino-partial-{}", partial_suffix()))
}

/// 远程文件条目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<u64>,
    pub permissions: u32,
}

/// SFTP 后台命令（UI → 后台）。
#[derive(Debug)]
pub enum SftpCmd {
    List { path: String },
    Upload { local: PathBuf, remote: String },
    Download { remote: String, local: PathBuf },
    Remove { path: String, is_dir: bool },
    Rename { from: String, to: String },
    Mkdir { path: String },
    Shutdown,
}

/// SFTP 事件（后台 → UI）。
#[derive(Debug, Clone)]
pub enum SftpEvent {
    /// 连接就绪，同时返回 SFTP 会话的初始目录。
    Ready { home: String },
    /// 连接失败。
    Failed(String),
    /// 目录列表完成。
    Listed {
        path: String,
        entries: Vec<RemoteEntry>,
    },
    /// 传输进度。
    Progress {
        label: String,
        done: u64,
        total: u64,
    },
    /// 操作完成。
    Done { label: String },
    /// 操作失败。
    Error { label: String, message: String },
    /// 连接关闭。
    Closed,
}

/// UI 持有的 SFTP 操作句柄。
#[derive(Clone)]
pub struct SftpHandle {
    cmd_tx: UnboundedSender<SftpCmd>,
}

impl SftpHandle {
    /// 从原始发送端构造（测试用）。
    pub fn from_raw(cmd_tx: UnboundedSender<SftpCmd>) -> Self {
        Self { cmd_tx }
    }
}

impl SftpHandle {
    /// 列出远程目录。
    pub fn list(&self, path: &str) {
        let _ = self.cmd_tx.send(SftpCmd::List {
            path: path.to_string(),
        });
    }

    /// 上传本地文件到远程。
    pub fn upload(&self, local: &Path, remote: &str) {
        let _ = self.cmd_tx.send(SftpCmd::Upload {
            local: local.to_path_buf(),
            remote: remote.to_string(),
        });
    }

    /// 下载远程文件到本地。
    pub fn download(&self, remote: &str, local: &Path) {
        let _ = self.cmd_tx.send(SftpCmd::Download {
            remote: remote.to_string(),
            local: local.to_path_buf(),
        });
    }

    /// 删除远程文件或目录。
    pub fn remove(&self, path: &str, is_dir: bool) {
        let _ = self.cmd_tx.send(SftpCmd::Remove {
            path: path.to_string(),
            is_dir,
        });
    }

    /// 重命名远程文件或目录。
    pub fn rename(&self, from: &str, to: &str) {
        let _ = self.cmd_tx.send(SftpCmd::Rename {
            from: from.to_string(),
            to: to.to_string(),
        });
    }

    /// 新建远程目录。
    pub fn mkdir(&self, path: &str) {
        let _ = self.cmd_tx.send(SftpCmd::Mkdir {
            path: path.to_string(),
        });
    }

    /// 关闭 SFTP 连接。
    pub fn close(&self) {
        let _ = self.cmd_tx.send(SftpCmd::Shutdown);
    }
}

/// 发起 SFTP 连接（非阻塞）。
///
/// 返回（线程句柄, 操作句柄, 事件接收端）。
pub fn connect_sftp(
    profile: &HostProfile,
) -> (std::thread::JoinHandle<()>, SftpHandle, Receiver<SftpEvent>) {
    // 事件流有界，避免非活动标签在大文件传输时无限积压进度事件。
    let (ev_tx, ev_rx) = mpsc::channel::<SftpEvent>(128);
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<SftpCmd>();
    let profile = profile.clone();
    let handle = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("创建 tokio runtime 失败");
        runtime.block_on(sftp_main(profile, cmd_rx, ev_tx));
    });
    (handle, SftpHandle { cmd_tx }, ev_rx)
}

/// 连接并运行 SFTP 操作循环（阻塞直到关闭）。
async fn sftp_main(
    profile: HostProfile,
    mut cmd_rx: UnboundedReceiver<SftpCmd>,
    ev_tx: Sender<SftpEvent>,
) {
    // ==================== 1. 连接与认证（含主机密钥 TOFU 校验） ====================
    log::info!("sftp_main 启动：{}:{}", profile.host, profile.port);
    let config = Arc::new(ssh_config());
    let mut handle = match connect_verified(config, &profile).await {
        Ok(h) => h,
        Err(e) => {
            let _ = ev_tx
                .send(SftpEvent::Failed(format!(
                    "连接 {}:{} 失败：{e}",
                    profile.host, profile.port
                )))
                .await;
            return;
        }
    };

    log::info!("TCP 连接成功，开始认证");
    let authed = match crate::ssh::authenticate(&mut handle, &profile).await {
        Ok(ok) => ok,
        Err(e) => {
            let _ = ev_tx.send(SftpEvent::Failed(e)).await;
            return;
        }
    };
    if !authed {
        let _ = ev_tx
            .send(SftpEvent::Failed("认证失败：用户名或密码/密钥错误".into()))
            .await;
        return;
    }

    // ==================== 2. 打开 SFTP subsystem ====================
    let channel = match handle.channel_open_session().await {
        Ok(c) => c,
        Err(e) => {
            let _ = ev_tx
                .send(SftpEvent::Failed(format!("打开会话失败：{e}")))
                .await;
            return;
        }
    };
    if let Err(e) = channel.request_subsystem(true, "sftp").await {
        let _ = ev_tx
            .send(SftpEvent::Failed(format!("启动 SFTP 子系统失败：{e}")))
            .await;
        return;
    }
    let sftp = match SftpSession::new(channel.into_stream()).await {
        Ok(s) => s,
        Err(e) => {
            let _ = ev_tx
                .send(SftpEvent::Failed(format!("初始化 SFTP 失败：{e}")))
                .await;
            return;
        }
    };

    log::info!("SFTP 连接就绪");
    let home = sftp
        .canonicalize(".")
        .await
        .unwrap_or_else(|_| "/".to_string());
    let _ = ev_tx.send(SftpEvent::Ready { home }).await;

    // ==================== 3. 操作循环 ====================
    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            SftpCmd::List { path } => {
                let result = list_dir(&sftp, &path).await;
                match result {
                    Ok(entries) => {
                        let _ = ev_tx.send(SftpEvent::Listed { path, entries }).await;
                    }
                    Err(e) => {
                        let _ = ev_tx
                            .send(SftpEvent::Error {
                                label: "列出目录".into(),
                                message: e,
                            })
                            .await;
                    }
                }
            }
            SftpCmd::Upload { local, remote } => {
                let label = format!(
                    "上传 {}",
                    local
                        .file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| local.display().to_string())
                );
                match upload_file(&sftp, &local, &remote, &label, &ev_tx).await {
                    Ok(()) => {
                        let _ = ev_tx.send(SftpEvent::Done { label }).await;
                    }
                    Err(e) => {
                        let _ = ev_tx.send(SftpEvent::Error { label, message: e }).await;
                    }
                }
            }
            SftpCmd::Download { remote, local } => {
                let label = format!(
                    "下载 {}",
                    Path::new(&remote)
                        .file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| remote.clone())
                );
                match download_file(&sftp, &remote, &local, &label, &ev_tx).await {
                    Ok(()) => {
                        let _ = ev_tx.send(SftpEvent::Done { label }).await;
                    }
                    Err(e) => {
                        let _ = ev_tx.send(SftpEvent::Error { label, message: e }).await;
                    }
                }
            }
            SftpCmd::Remove { path, is_dir } => {
                let label = format!(
                    "删除 {}",
                    Path::new(&path)
                        .file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.clone())
                );
                let result = if is_dir {
                    sftp.remove_dir(&path).await
                } else {
                    sftp.remove_file(&path).await
                };
                match result {
                    Ok(()) => {
                        let _ = ev_tx.send(SftpEvent::Done { label }).await;
                    }
                    Err(e) => {
                        let _ = ev_tx
                            .send(SftpEvent::Error {
                                label,
                                message: e.to_string(),
                            })
                            .await;
                    }
                }
            }
            SftpCmd::Rename { from, to } => {
                let label = format!("重命名 {}", from);
                match sftp.rename(&from, &to).await {
                    Ok(()) => {
                        let _ = ev_tx.send(SftpEvent::Done { label }).await;
                    }
                    Err(e) => {
                        let _ = ev_tx
                            .send(SftpEvent::Error {
                                label,
                                message: e.to_string(),
                            })
                            .await;
                    }
                }
            }
            SftpCmd::Mkdir { path } => {
                let label = format!("新建目录 {}", path);
                match sftp.create_dir(&path).await {
                    Ok(()) => {
                        let _ = ev_tx.send(SftpEvent::Done { label }).await;
                    }
                    Err(e) => {
                        let _ = ev_tx
                            .send(SftpEvent::Error {
                                label,
                                message: e.to_string(),
                            })
                            .await;
                    }
                }
            }
            SftpCmd::Shutdown => break,
        }
    }
    let _ = ev_tx.send(SftpEvent::Closed).await;
}

/// 列出远程目录。
async fn list_dir(sftp: &SftpSession, path: &str) -> Result<Vec<RemoteEntry>, String> {
    let mut entries = Vec::new();
    for entry in sftp.read_dir(path).await.map_err(|e| e.to_string())? {
        let meta = entry.metadata();
        entries.push(RemoteEntry {
            name: entry.file_name(),
            is_dir: meta.is_dir(),
            size: meta.len(),
            modified: meta.mtime.map(u64::from),
            permissions: meta.permissions.map(u64::from).unwrap_or(0) as u32,
        });
    }
    // 目录优先，按名称排序。
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(entries)
}

/// 上传本地文件到远程（带进度）。
async fn upload_file(
    sftp: &SftpSession,
    local: &Path,
    remote: &str,
    label: &str,
    ev_tx: &Sender<SftpEvent>,
) -> Result<(), String> {
    let partial = partial_remote_path(remote);
    let result = async {
        let mut local_file = tokio::fs::File::open(local)
            .await
            .map_err(|e| e.to_string())?;
        let total = local_file
            .metadata()
            .await
            .map_err(|e| e.to_string())?
            .len();
        let mut remote_file = sftp.create(&partial).await.map_err(|e| e.to_string())?;

        let mut done = 0u64;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = local_file.read(&mut buf).await.map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            remote_file
                .write_all(&buf[..n])
                .await
                .map_err(|e| e.to_string())?;
            done += n as u64;
            // 进度是可丢弃的最新状态；有界队列满时不阻塞传输线程。
            let _ = ev_tx.try_send(SftpEvent::Progress {
                label: label.to_string(),
                done,
                total,
            });
        }
        remote_file.close().await.map_err(|e| e.to_string())
    }
    .await;
    if let Err(error) = result {
        let _ = sftp.remove_file(&partial).await;
        return Err(error);
    }
    if let Err(error) = sftp.rename(&partial, remote).await {
        let _ = sftp.remove_file(&partial).await;
        return Err(error.to_string());
    }
    Ok(())
}

/// 下载远程文件到本地（带进度）。
async fn download_file(
    sftp: &SftpSession,
    remote: &str,
    local: &Path,
    label: &str,
    ev_tx: &Sender<SftpEvent>,
) -> Result<(), String> {
    let partial = partial_local_path(local);
    let result = async {
        let meta = sftp.metadata(remote).await.map_err(|e| e.to_string())?;
        let total = meta.len();
        let mut remote_file = sftp.open(remote).await.map_err(|e| e.to_string())?;
        let mut local_file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&partial)
            .await
            .map_err(|e| e.to_string())?;

        let mut done = 0u64;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = remote_file
                .read(&mut buf)
                .await
                .map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            local_file
                .write_all(&buf[..n])
                .await
                .map_err(|e| e.to_string())?;
            done += n as u64;
            let _ = ev_tx.try_send(SftpEvent::Progress {
                label: label.to_string(),
                done,
                total,
            });
        }
        local_file.flush().await.map_err(|e| e.to_string())?;
        local_file.sync_all().await.map_err(|e| e.to_string())
    }
    .await;
    if let Err(error) = result {
        let _ = tokio::fs::remove_file(&partial).await;
        return Err(error);
    }
    if let Err(error) = tokio::fs::rename(&partial, local).await {
        let _ = tokio::fs::remove_file(&partial).await;
        return Err(error.to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 传输临时文件与目标同目录() {
        let local = Path::new("/tmp/result.txt");
        let partial = partial_local_path(local);
        assert_eq!(partial.parent(), local.parent());
        assert!(partial
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(".result.txt.mino-partial-")));

        let remote = partial_remote_path("/srv/result.txt");
        assert!(remote.starts_with("/srv/result.txt.mino-partial-"));
    }
}
