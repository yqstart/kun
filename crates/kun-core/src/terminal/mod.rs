//! 终端会话：PTY + VT 仿真封装（基于 alacritty_terminal）。
//!
//! 本地会话由 alacritty 的 EventLoop 驱动（PTY 读取线程）；
//! 远程会话由自建 tokio 读循环驱动（SSH channel 数据 → vte 解析器）。

pub mod keys;

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, Msg};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{self, Term};
use alacritty_terminal::tty;

pub use alacritty_terminal::term::TermMode;

/// 终端尺寸（实现 alacritty 的 `Dimensions`）。
#[derive(Clone, Copy, Debug)]
pub struct TermSize {
    pub rows: usize,
    pub cols: usize,
}

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

/// 会话事件（后台线程 → UI 线程的通知）。
#[derive(Debug, Clone)]
pub enum SessionEvent {
    /// 终端有新内容，需要重绘。
    Wakeup,
    /// 窗口标题变化。
    Title(String),
    /// 子进程退出。
    ChildExit,
    /// 终端请求应用写入数据（如粘贴内容回传）。
    PtyWrite(String),
    /// 终端铃响。
    Bell,
}

/// 事件回调：后台有数据时由监听器线程调用（用于触发 UI 重绘）。
pub type EventHandler = Arc<dyn Fn(&SessionEvent) + Send + Sync>;

/// 会话共享状态（监听器与 UI 线程共同访问）。
#[derive(Default)]
pub(crate) struct Shared {
    pub(crate) title: Mutex<String>,
    pub(crate) exited: Mutex<bool>,
    pub(crate) pending: Mutex<Vec<SessionEvent>>,
}

/// alacritty 事件监听器：把事件记录到共享状态并通知回调。
pub struct Listener {
    pub(crate) shared: Arc<Shared>,
    pub(crate) on_event: EventHandler,
}

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        let mut pending = self.shared.pending.lock().unwrap();
        match event {
            Event::Wakeup => pending.push(SessionEvent::Wakeup),
            Event::Title(title) => {
                *self.shared.title.lock().unwrap() = title.clone();
                pending.push(SessionEvent::Title(title));
            }
            Event::ChildExit(_) => {
                *self.shared.exited.lock().unwrap() = true;
                pending.push(SessionEvent::ChildExit);
            }
            Event::PtyWrite(text) => pending.push(SessionEvent::PtyWrite(text)),
            Event::Bell => pending.push(SessionEvent::Bell),
            _ => {}
        }
        if let Some(event) = pending.last() {
            (self.on_event)(event);
        }
    }
}

/// 写目标：非阻塞写入字节到会话（本地 → EventLoopSender，远程 → 命令队列）。
pub type WriteFn = Arc<dyn Fn(&[u8]) + Send + Sync>;

#[derive(Clone)]
pub struct Writer(WriteFn);

impl Writer {
    pub fn new(f: impl Fn(&[u8]) + Send + Sync + 'static) -> Self {
        Self(Arc::new(f))
    }

    pub fn write(&self, bytes: &[u8]) {
        (self.0)(bytes);
    }
}

/// 尺寸调整目标：非阻塞通知后台调整窗口尺寸。
#[derive(Clone)]
pub struct Resizer(Arc<dyn Fn(u16, u16) + Send + Sync>);

impl Resizer {
    pub fn new(f: impl Fn(u16, u16) + Send + Sync + 'static) -> Self {
        Self(Arc::new(f))
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        (self.0)(cols, rows);
    }
}

/// 关闭目标。
#[derive(Clone)]
pub struct Shuttor(Arc<dyn Fn() + Send + Sync>);

impl Shuttor {
    pub fn new(f: impl Fn() + Send + Sync + 'static) -> Self {
        Self(Arc::new(f))
    }

    pub fn shutdown(&self) {
        (self.0)();
    }
}

/// 终端会话（本地或远程 shell 的统一封装）。
pub struct Session {
    term: Arc<FairMutex<Term<Listener>>>,
    shared: Arc<Shared>,
    writer: Writer,
    resizer: Resizer,
    shuttor: Shuttor,
    /// 是否为远程会话（远程不启用本地补全：无本机文件系统对应）。
    is_remote: bool,
    /// 本地会话的 PTY 读取线程（远程会话为 None）。
    pty_thread: Option<
        JoinHandle<(
            EventLoop<tty::Pty, Listener>,
            alacritty_terminal::event_loop::State,
        )>,
    >,
}

/// 会话创建参数（本地）。
#[derive(Default)]
pub struct SessionOptions {
    /// 要启动的 shell 程序，None 表示系统默认 shell。
    pub shell: Option<String>,
    /// 启动目录。
    pub working_directory: Option<PathBuf>,
    /// 附加环境变量（追加到继承的环境，alacritty 为覆盖语义）。
    pub env: HashMap<String, String>,
}

impl Session {
    /// 构造会话（由本地/远程创建逻辑调用）。
    pub(crate) fn new(
        term: Arc<FairMutex<Term<Listener>>>,
        shared: Arc<Shared>,
        writer: Writer,
        resizer: Resizer,
        shuttor: Shuttor,
        is_remote: bool,
    ) -> Session {
        Session {
            term,
            shared,
            writer,
            resizer,
            shuttor,
            is_remote,
            pty_thread: None,
        }
    }

    /// 是否为远程会话（决定本地补全等本机能力是否可用）。
    pub fn is_remote(&self) -> bool {
        self.is_remote
    }

    /// 启动一个本地 PTY 会话。
    ///
    /// `on_event` 会在 PTY 线程收到数据时被调用（UI 层用它请求重绘）。
    pub fn spawn_local(
        options: SessionOptions,
        cols: u16,
        rows: u16,
        on_event: EventHandler,
    ) -> io::Result<Session> {
        let shared = Arc::new(Shared::default());
        let config = term::Config::default();

        // 创建 PTY（macOS/Unix 平台）。
        let shell = options
            .shell
            .map(|program| tty::Shell::new(program, Vec::new()));
        let pty_options = tty::Options {
            shell,
            working_directory: options.working_directory,
            drain_on_exit: true,
            env: options.env,
        };
        let window_size = WindowSize {
            num_lines: rows,
            num_cols: cols,
            cell_width: 1,
            cell_height: 1,
        };
        let pty = tty::new(&pty_options, window_size, 0)?;

        // 创建终端状态机。
        let term = Arc::new(FairMutex::new(Term::new(
            config,
            &TermSize {
                rows: rows as usize,
                cols: cols as usize,
            },
            Listener {
                shared: shared.clone(),
                on_event: Arc::new(|_| {}),
            },
        )));

        // 创建事件循环（PTY 读取线程）。
        let event_loop = EventLoop::new(
            term.clone(),
            Listener {
                shared: shared.clone(),
                on_event: on_event.clone(),
            },
            pty,
            true,
            false,
        )?;
        let channel = event_loop.channel();
        let pty_thread: JoinHandle<(
            EventLoop<tty::Pty, Listener>,
            alacritty_terminal::event_loop::State,
        )> = event_loop.spawn();

        // 写 / 缩放 / 关闭 都走 EventLoopSender（克隆共享）。
        let writer_channel = channel.clone();
        let resizer_channel = channel.clone();
        let shuttor_channel = channel.clone();
        let writer = Writer::new(move |bytes: &[u8]| {
            let _ = writer_channel.send(Msg::Input(bytes.to_vec().into()));
        });
        let resizer = Resizer::new(move |cols: u16, rows: u16| {
            let _ = resizer_channel.send(Msg::Resize(WindowSize {
                num_lines: rows,
                num_cols: cols,
                cell_width: 1,
                cell_height: 1,
            }));
        });
        let shuttor = Shuttor::new(move || {
            let _ = shuttor_channel.send(Msg::Shutdown);
        });

        let mut session = Session::new(term, shared, writer, resizer, shuttor, false);
        session.pty_thread = Some(pty_thread);
        Ok(session)
    }

    /// PTY 读取线程是否已退出（用于诊断写入失效问题）。
    pub fn pty_thread_finished(&self) -> bool {
        self.pty_thread
            .as_ref()
            .map(|t| t.is_finished())
            .unwrap_or(false)
    }

    /// 访问终端状态机（渲染时锁定读取）。
    pub fn term(&self) -> Arc<FairMutex<Term<Listener>>> {
        self.term.clone()
    }

    /// 写入数据（键盘输入、粘贴内容等）。
    pub fn write(&self, bytes: &[u8]) {
        self.writer.write(bytes);
    }

    /// 调整终端尺寸（窗口 resize 时调用）。
    pub fn resize(&mut self, cols: u16, rows: u16) {
        // 同步更新终端状态机网格。
        self.term.lock().resize(TermSize {
            rows: rows as usize,
            cols: cols as usize,
        });
        // 通知后台（PTY/SSH channel）。
        self.resizer.resize(cols, rows);
    }

    /// 取出所有待处理事件（UI 每帧轮询）。
    pub fn drain_events(&self) -> Vec<SessionEvent> {
        let mut pending = self.shared.pending.lock().unwrap();
        std::mem::take(&mut *pending)
    }

    /// 当前窗口标题。
    pub fn title(&self) -> String {
        self.shared.title.lock().unwrap().clone()
    }

    /// 子进程是否已退出。
    pub fn has_exited(&self) -> bool {
        *self.shared.exited.lock().unwrap()
    }

    /// 关闭会话。
    pub fn shutdown(self) {
        self.shuttor.shutdown();
    }
}

impl Drop for Session {
    /// 会话被丢弃时优雅关闭后台线程。
    fn drop(&mut self) {
        self.shuttor.shutdown();
    }
}
