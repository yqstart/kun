//! SSH 远程会话：远程终端 + SFTP（基于 russh / russh-sftp）。
//!
//! 远程终端与本地终端共用 `Term` 状态机：后台 tokio 任务读 SSH channel，
//! 数据经 vte 解析器喂入 `Term`，写操作走命令队列（UI 线程非阻塞）。

pub mod sftp;

use std::path::PathBuf;
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

/// russh 客户端 Handler（MVP：接受所有服务器密钥）。
pub(crate) struct ClientHandler;

impl client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &russh::keys::PublicKey,
    ) -> Result<bool, Self::Error> {
        Ok(true)
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
            // ============ 1. TCP 连接与认证 ============
            let config = Arc::new(client::Config::default());
            let mut handle =
                match client::connect(config, (profile.host.as_str(), profile.port), ClientHandler)
                    .await
                {
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
                term, shared, writer, resizer, shuttor,
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
    _handle: client::Handle<ClientHandler>,
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

/// 加载私钥文件（支持口令）。
fn load_private_key(
    path: &PathBuf,
    passphrase: Option<&str>,
) -> Result<russh::keys::PrivateKey, String> {
    let content = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    decode_secret_key(&content, passphrase).map_err(|e| e.to_string())
}

/// 执行认证（密码或私钥），返回是否成功。
pub(crate) async fn authenticate(
    handle: &mut client::Handle<ClientHandler>,
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
