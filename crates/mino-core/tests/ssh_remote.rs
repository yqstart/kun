//! 远程终端集成测试：连接本地测试 sshd（127.0.0.1:2222，公钥认证）。
//!
//! 运行前需启动测试服务器：
//! ```bash
//! /usr/sbin/sshd -f /tmp/mino-test-sshd/sshd_config
//! ```

use std::sync::Arc;
use std::time::{Duration, Instant};

use mino_core::config::{Auth, HostProfile};
use mino_core::ssh::{connect_remote, ConnectResult};
use mino_core::terminal::{Session, SessionEvent};

/// 测试主机（环境变量可覆盖，默认本地测试 sshd）。
fn test_profile() -> HostProfile {
    let key_path = std::env::var("MINO_TEST_KEY").unwrap_or_else(|_| {
        format!(
            "{}/.ssh/id_ed25519",
            std::env::var("HOME").unwrap_or_default()
        )
    });
    HostProfile {
        name: "集成测试".into(),
        host: std::env::var("MINO_TEST_HOST").unwrap_or_else(|_| "127.0.0.1".into()),
        port: std::env::var("MINO_TEST_PORT")
            .unwrap_or_else(|_| "2222".into())
            .parse()
            .unwrap(),
        user: std::env::var("MINO_TEST_USER").unwrap_or_else(|_| whoami()),
        auth: Auth::Key {
            path: key_path.into(),
            passphrase: None,
        },
    }
}

/// 当前用户名。
fn whoami() -> String {
    std::env::var("USER").unwrap_or_else(|_| "root".into())
}

/// 将终端可见区域转为文本（用于断言输出）。
fn grid_text(session: &Session) -> String {
    use alacritty_terminal::term::cell::Flags;
    let term_arc = session.term();
    let guard = term_arc.lock();
    let content = guard.renderable_content();
    let mut lines: Vec<String> = Vec::new();
    let mut current: String = String::new();
    let mut current_line = usize::MAX;
    for item in content.display_iter {
        let cell = item.cell;
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) || cell.flags.contains(Flags::HIDDEN) {
            continue;
        }
        if item.point.line.0 as usize != current_line {
            if current_line != usize::MAX {
                lines.push(current.trim_end().to_string());
            }
            current = String::new();
            current_line = item.point.line.0 as usize;
        }
        current.push(cell.c);
    }
    if current_line != usize::MAX {
        lines.push(current.trim_end().to_string());
    }
    lines.join("\n")
}

/// 等待终端输出包含指定子串。
fn wait_for_text(session: &Session, needle: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let text = grid_text(session);
        if text.contains(needle) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// 将测试 sshd 的 known_hosts 记录与 hostkey 放在同一目录（/tmp/mino-test-sshd）：
/// hostkey 随 /tmp 清理重建时指纹记录一并消失，避免旧指纹不匹配导致测试失败。
/// `call_once` 保证进程内只设置一次（测试并行安全）。
static KNOWN_HOSTS_INIT: std::sync::Once = std::sync::Once::new();
fn init_test_env() {
    KNOWN_HOSTS_INIT.call_once(|| {
        std::env::set_var("MINO_KNOWN_HOSTS", "/tmp/mino-test-sshd/known_hosts.toml");
    });
}

/// 测试 sshd 未启动时跳过集成测试，避免把环境缺失误报为产品回归。
fn sshd_available(profile: &HostProfile) -> bool {
    use std::net::ToSocketAddrs;

    (profile.host.as_str(), profile.port)
        .to_socket_addrs()
        .map(|addresses| {
            addresses.into_iter().any(|address| {
                std::net::TcpStream::connect_timeout(&address, Duration::from_millis(500)).is_ok()
            })
        })
        .unwrap_or(false)
}

#[test]
fn 远程终端_连接_执行命令_收到输出() {
    init_test_env();
    let profile = test_profile();
    if !sshd_available(&profile) {
        eprintln!(
            "跳过：测试 sshd 未监听 {}:{}（scripts/test-sshd.sh start）",
            profile.host, profile.port
        );
        return;
    }
    if !auth_key_path_exists(&profile) {
        eprintln!("跳过：测试私钥不存在");
        return;
    }

    let _ = env_logger::builder().is_test(true).try_init();
    let on_event = Arc::new(|_ev: &SessionEvent| {});
    let (_thread, mut rx) = connect_remote(&profile, 80, 24, on_event);

    // 等待连接完成。
    let deadline = Instant::now() + Duration::from_secs(10);
    let session = loop {
        match rx.try_recv() {
            Ok(ConnectResult::Connected(session)) => break session,
            Ok(ConnectResult::Failed(e)) => {
                panic!("连接失败：{e}");
            }
            Err(_) => {
                assert!(Instant::now() < deadline, "连接超时");
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    };

    // 发送命令并等待输出回显。
    session.write(b"echo MINO_SSH_OK\n");
    assert!(
        wait_for_text(&session, "MINO_SSH_OK", Duration::from_secs(10)),
        "未收到命令回显，终端内容：\n{}",
        grid_text(&session)
    );
}

// 检查密钥文件是否存在。
fn auth_key_path_exists(profile: &HostProfile) -> bool {
    match &profile.auth {
        Auth::Key { path, .. } => path.exists(),
        _ => false,
    }
}
