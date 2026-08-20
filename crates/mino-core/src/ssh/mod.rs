//! SSH 远程会话：远程终端 + SFTP（基于 russh / russh-sftp）。
//!
//! 远程终端与本地终端共用 `Term` 状态机：后台 tokio 任务读 SSH channel，
//! 数据经 vte 解析器喂入 `Term`，写操作走命令队列（UI 线程非阻塞）。

pub(crate) mod known_hosts;
pub mod sftp;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{self, Term};
use alacritty_terminal::vte::ansi::Processor;
use russh::client;
use russh::keys::{decode_secret_key, HashAlg, PrivateKeyWithHashAlg};
use russh::{Channel, ChannelMsg};
use tokio::sync::mpsc::{self, UnboundedReceiver};

use crate::config::Auth;
use crate::terminal::{EventHandler, Listener, Session, SessionEvent, Shared, TermSize};

use known_hosts::{default_known_hosts_path, HostKeyVerifier};

/// 远程会话后台命令。
enum SessionCmd {
    /// 写入字节。
    Write(Vec<u8>),
    /// 调整终端尺寸。
    Resize(u16, u16),
    /// 关闭。
    Shutdown,
}

/// 连接结果（异步任务 → UI 线程）。
pub enum ConnectResult {
    /// 连接成功，返回会话。
    Connected(Session),
    /// 连接失败，返回错误信息。
    Failed(String),
}

/// 统一的 SSH 客户端配置：30 秒无数据发 keepalive，3 次无响应断开
/// （空闲连接被中间设备静默断开后能及时发现，避免会话假死）。
fn ssh_config() -> client::Config {
    client::Config {
        keepalive_interval: Some(std::time::Duration::from_secs(30)),
        keepalive_max: 3,
        ..Default::default()
    }
}

/// 连接并校验服务器密钥（TOFU），失败时返回带原因的错误。
pub(crate) async fn connect_verified(
    config: Arc<client::Config>,
    profile: &crate::config::HostProfile,
) -> Result<client::Handle<HostKeyVerifier>, String> {
    let (verifier, verifier_error) = HostKeyVerifier::new(
        profile.host.clone(),
        profile.port,
        default_known_hosts_path(),
    );
    match client::connect(config, (profile.host.as_str(), profile.port), verifier).await {
        Ok(handle) => Ok(handle),
        Err(e) => {
            // 密钥校验失败时给出明确原因（指纹不匹配 + 修复指引）；
            // 其他错误（TCP 拒绝等）沿用 russh 原文。
            let detail = verifier_error.lock().unwrap().take();
            Err(detail.unwrap_or_else(|| e.to_string()))
        }
    }
}

/// 发起远程会话连接（非阻塞，立即返回）。
///
/// 连接在后台线程完成；结果通过返回的 receiver 接收。
/// 后台线程的 tokio runtime 存活到会话关闭（remote_loop 结束时）。
pub fn connect_remote(
    profile: &crate::config::HostProfile,
    cols: u16,
    rows: u16,
    on_event: EventHandler,
) -> (
    std::thread::JoinHandle<()>,
    UnboundedReceiver<ConnectResult>,
) {
    let (tx, rx) = mpsc::unbounded_channel();
    let profile = profile.clone();
    let handle = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("创建 tokio runtime 失败");
        let session_done = Arc::new(tokio::sync::Notify::new());
        let session_done_loop = session_done.clone();
        runtime.block_on(async move {
            // ============ 1. TCP 连接与认证（含主机密钥 TOFU 校验） ============
            let config = Arc::new(ssh_config());
            let mut handle = match connect_verified(config, &profile).await {
                Ok(h) => h,
                Err(e) => {
                    let _ = tx.send(ConnectResult::Failed(format!(
                        "连接 {}:{} 失败：{e}",
                        profile.host, profile.port
                    )));
                    return;
                }
            };

            let authed = match authenticate(&mut handle, &profile).await {
                Ok(ok) => ok,
                Err(e) => {
                    let _ = tx.send(ConnectResult::Failed(e));
                    return;
                }
            };
            if !authed {
                let _ = tx.send(ConnectResult::Failed(
                    "认证失败：用户名或密码/密钥错误".into(),
                ));
                return;
            }

            // ============ 2. 打开 shell channel ============
            let channel = match handle.channel_open_session().await {
                Ok(c) => c,
                Err(e) => {
                    let _ = tx.send(ConnectResult::Failed(format!("打开会话失败：{e}")));
                    return;
                }
            };
            if let Err(e) = channel
                .request_pty(true, "xterm-256color", cols as u32, rows as u32, 0, 0, &[])
                .await
            {
                let _ = tx.send(ConnectResult::Failed(format!("申请 PTY 失败：{e}")));
                return;
            }
            if let Err(e) = channel.request_shell(true).await {
                let _ = tx.send(ConnectResult::Failed(format!("启动 shell 失败：{e}")));
                return;
            }

            // ============ 3. 创建终端状态机 ============
            let shared = Arc::new(Shared::default());
            let term: Arc<FairMutex<Term<Listener>>> = Arc::new(FairMutex::new(Term::new(
                term::Config::default(),
                &TermSize {
                    rows: rows as usize,
                    cols: cols as usize,
                },
                Listener {
                    shared: shared.clone(),
                    on_event: on_event.clone(),
                },
            )));

            // ============ 4. 命令队列（UI → 后台） ============
            let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<SessionCmd>();

            // ============ 5. 后台读循环（持有 handle 保持连接） ============
            let remote_term = term.clone();
            let remote_shared = shared.clone();
            let remote_on_event = on_event.clone();
            tokio::spawn(async move {
                remote_loop(
                    channel,
                    cmd_rx,
                    remote_term,
                    remote_shared,
                    remote_on_event,
                    handle,
                )
                .await;
                session_done_loop.notify_one();
            });

            // ============ 6. 组装会话 ============
            let writer_tx = cmd_tx.clone();
            let resizer_tx = cmd_tx.clone();
            let shuttor_tx = cmd_tx.clone();
            let writer = crate::terminal::Writer::new(move |bytes: &[u8]| {
                let _ = writer_tx.send(SessionCmd::Write(bytes.to_vec()));
            });
            let resizer = crate::terminal::Resizer::new(move |cols: u16, rows: u16| {
                let _ = resizer_tx.send(SessionCmd::Resize(cols, rows));
            });
            let shuttor = crate::terminal::Shuttor::new(move || {
                let _ = shuttor_tx.send(SessionCmd::Shutdown);
            });

            let _ = tx.send(ConnectResult::Connected(Session::new(
                term, shared, writer, resizer, shuttor, true, None,
            )));

            // ============ 7. 等待会话关闭，保持 runtime 存活 ============
            session_done.notified().await;
        });
    });
    (handle, rx)
}

/// 远程终端后台循环：读 channel 数据喂解析器，消费命令队列。
async fn remote_loop(
    mut channel: Channel<russh::client::Msg>,
    mut cmd_rx: UnboundedReceiver<SessionCmd>,
    term: Arc<FairMutex<Term<Listener>>>,
    shared: Arc<Shared>,
    on_event: EventHandler,
    _handle: client::Handle<HostKeyVerifier>,
) {
    log::info!("remote_loop 启动");
    let mut parser: alacritty_terminal::vte::ansi::Processor = Processor::new();

    loop {
        tokio::select! {
            // 命令队列：UI 线程写入/缩放。
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(SessionCmd::Write(bytes)) => {
                        log::debug!("远程写入 {} 字节", bytes.len());
                        if let Err(e) = channel.data(&bytes[..]).await {
                            log::warn!("写入远程终端失败：{e}");
                            break;
                        }
                    }
                    Some(SessionCmd::Resize(cols, rows)) => {
                        let _ = channel.window_change(cols as u32, rows as u32, 0, 0).await;
                    }
                    Some(SessionCmd::Shutdown) | None => {
                        break;
                    }
                }
            }
            // channel 数据：远程输出 → vte 解析 → Term。
            msg = channel.wait() => {
                match msg {
                    Some(ChannelMsg::Data { data }) => {
                        log::debug!("远程收到 {} 字节", data.len());
                        let mut guard = term.lock();
                        parser.advance(&mut *guard, &data);
                        drop(guard);
                        (on_event)(&SessionEvent::Wakeup);
                    }
                    Some(ChannelMsg::ExtendedData { data, .. }) => {
                        // stderr 也喂入解析器（保持输出顺序完整）。
                        let mut guard = term.lock();
                        parser.advance(&mut *guard, &data);
                        drop(guard);
                        (on_event)(&SessionEvent::Wakeup);
                    }
                    Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) => {
                        break;
                    }
                    Some(_) => {}
                    None => {
                        break;
                    }
                }
            }
        }
    }

    *shared.exited.lock().unwrap() = true;
    (on_event)(&SessionEvent::ChildExit);
}

/// 展开私钥路径中的 `~`（配置里用户常写 `~/.ssh/id_ed25519`，
/// 而 `std::fs` 不展开波浪号，会导致加载失败）。
fn expand_tilde(path: &Path) -> PathBuf {
    let Some(s) = path.to_str() else {
        return path.to_path_buf();
    };
    let home = std::env::var("HOME").ok().map(PathBuf::from);
    if s == "~" {
        if let Some(h) = home {
            return h;
        }
    } else if let Some(rest) = s.strip_prefix("~/") {
        if let Some(h) = home {
            return h.join(rest);
        }
    }
    path.to_path_buf()
}

/// 加载私钥文件（支持口令，路径自动展开 `~`）。
fn load_private_key(
    path: &Path,
    passphrase: Option<&str>,
) -> Result<russh::keys::PrivateKey, String> {
    let expanded = expand_tilde(path);
    let content = std::fs::read_to_string(&expanded)
        .map_err(|e| format!("读取私钥 {} 失败：{e}", expanded.display()))?;
    decode_secret_key(&content, passphrase).map_err(|e| e.to_string())
}

/// 执行认证（密码或私钥），返回是否成功。
pub(crate) async fn authenticate(
    handle: &mut client::Handle<HostKeyVerifier>,
    profile: &crate::config::HostProfile,
) -> Result<bool, String> {
    let authed = match &profile.auth {
        Auth::Password(password) => handle
            .authenticate_password(&profile.user, password)
            .await
            .map_err(|e| format!("认证失败：{e}"))?
            .success(),
        Auth::Key { path, passphrase } => {
            let key = load_private_key(path, passphrase.as_deref())
                .map_err(|e| format!("加载私钥失败：{e}"))?;
            let key = PrivateKeyWithHashAlg::new(Arc::new(key), Some(HashAlg::Sha256));
            handle
                .authenticate_publickey(&profile.user, key)
                .await
                .map_err(|e| format!("公钥认证失败：{e}"))?
                .success()
        }
    };
    Ok(authed)
}
