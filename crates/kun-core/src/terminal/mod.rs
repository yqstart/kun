//! 终端会话：PTY + VT 仿真封装（基于 alacritty_terminal）。

pub mod keys;

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg};
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

/// 会话事件（PTY 线程 → UI 线程的通知）。
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

/// 事件回调：PTY 有数据时由监听器线程调用（用于触发 UI 重绘）。
pub type EventHandler = Arc<dyn Fn(&SessionEvent) + Send + Sync>;

/// 会话共享状态（监听器与 UI 线程共同访问）。
#[derive(Default)]
struct Shared {
    title: Mutex<String>,
    exited: Mutex<bool>,
    pending: Mutex<Vec<SessionEvent>>,
}

/// alacritty 事件监听器：把事件记录到共享状态并通知回调。
pub struct Listener {
    shared: Arc<Shared>,
    on_event: EventHandler,
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

/// 终端会话（本地或远程 shell 的统一封装）。
pub struct Session {
    term: Arc<FairMutex<Term<Listener>>>,
    channel: EventLoopSender,
    thread: JoinHandle<(EventLoop<tty::Pty, Listener>, alacritty_terminal::event_loop::State)>,
    shared: Arc<Shared>,
    cols: u16,
    rows: u16,
}

/// 会话创建参数。
pub struct SessionOptions {
    /// 要启动的 shell 程序，None 表示系统默认 shell。
    pub shell: Option<String>,
    /// 启动目录。
    pub working_directory: Option<PathBuf>,
}

impl Default for SessionOptions {
    fn default() -> Self {
        Self { shell: None, working_directory: None }
    }
}

impl Session {
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
        let listener = Listener { shared: shared.clone(), on_event };
        let config = term::Config::default();

        // 创建 PTY（macOS/Unix 平台）。
        let shell = options.shell.map(|program| tty::Shell::new(program, Vec::new()));
        let pty_options = tty::Options {
            shell,
            working_directory: options.working_directory,
            drain_on_exit: true,
            env: HashMap::new(),
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
            &TermSize { rows: rows as usize, cols: cols as usize },
            listener,
        )));

        // 创建事件循环（PTY 读取线程）。
        let event_loop = EventLoop::new(
            term.clone(),
            Listener { shared: shared.clone(), on_event: Arc::new(|_| {}) },
            pty,
            true,
            false,
        )?;
        let channel = event_loop.channel();
        let thread = event_loop.spawn();

        Ok(Session { term, channel, thread, shared, cols, rows })
    }

    /// 访问终端状态机（渲染时锁定读取）。
    pub fn term(&self) -> Arc<FairMutex<Term<Listener>>> {
        self.term.clone()
    }

    /// 写入数据到 PTY（键盘输入、粘贴内容等）。
    pub fn write(&self, bytes: &[u8]) {
        let _ = self.channel.send(Msg::Input(bytes.to_vec().into()));
    }

    /// 调整终端尺寸（窗口 resize 时调用）。
    pub fn resize(&mut self, cols: u16, rows: u16) {
        if cols == self.cols && rows == self.rows {
            return;
        }
        self.cols = cols;
        self.rows = rows;
        let _ = self.channel.send(Msg::Resize(WindowSize {
            num_lines: rows,
            num_cols: cols,
            cell_width: 1,
            cell_height: 1,
        }));
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

    /// 关闭会话：发送退出消息并等待 PTY 线程结束。
    pub fn shutdown(self) {
        let _ = self.channel.send(Msg::Shutdown);
        let _ = self.thread.join();
    }
}
