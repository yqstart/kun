//! SFTP 集成测试：连接本地测试 sshd，验证列表/上传/下载/删除/重命名/新建目录。

use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedReceiver;

use kun_core::config::{Auth, HostProfile};
use kun_core::ssh::sftp::{connect_sftp, SftpEvent};

/// 测试主机（与 ssh_remote.rs 相同的测试 sshd）。
fn test_profile() -> HostProfile {
    let key_path = std::env::var("KUN_TEST_KEY").unwrap_or_else(|_| {
        format!(
            "{}/.ssh/id_ed25519",
            std::env::var("HOME").unwrap_or_default()
        )
    });
    HostProfile {
        name: "SFTP 集成测试".into(),
        host: std::env::var("KUN_TEST_HOST").unwrap_or_else(|_| "127.0.0.1".into()),
        port: std::env::var("KUN_TEST_PORT")
            .unwrap_or_else(|_| "2222".into())
            .parse()
            .unwrap(),
        user: std::env::var("KUN_TEST_USER")
            .unwrap_or_else(|_| std::env::var("USER").unwrap_or_else(|_| "root".into())),
        auth: Auth::Key {
            path: key_path.into(),
            passphrase: None,
        },
    }
}

/// 等待特定事件出现（返回收到的所有事件）。
fn wait_event(
    rx: &mut UnboundedReceiver<SftpEvent>,
    timeout: Duration,
    predicate: impl Fn(&SftpEvent) -> bool,
) -> Result<Vec<SftpEvent>, String> {
    let deadline = Instant::now() + timeout;
    let mut seen = Vec::new();
    while Instant::now() < deadline {
        match rx.try_recv() {
            Ok(ev) => {
                seen.push(ev.clone());
                if predicate(&ev) {
                    return Ok(seen);
                }
            }
            Err(_) => std::thread::sleep(Duration::from_millis(30)),
        }
    }
    Err(format!("等待事件超时，已收到：{seen:?}"))
}

#[test]
fn sftp_完整操作流程() {
    let profile = test_profile();
    if let Auth::Key { path, .. } = &profile.auth {
        if !path.exists() {
            eprintln!("跳过：测试私钥不存在");
            return;
        }
    }

    let _ = env_logger::builder().is_test(true).try_init();
    let (_thread, handle, mut rx) = connect_sftp(&profile);

    // ============ 1. 连接就绪 ============
    let mut seen = wait_event(&mut rx, Duration::from_secs(10), |ev| {
        matches!(ev, SftpEvent::Ready)
    })
    .expect("SFTP 连接失败");
    if let Some(SftpEvent::Failed(e)) = seen.iter().find(|e| matches!(e, SftpEvent::Failed(_))) {
        panic!("SFTP 连接失败：{e:?}");
    }
    seen.clear();

    // ============ 2. 列出家目录 ============
    let remote_home = format!("/Users/{}", profile.user);
    handle.list(&remote_home);
    let seen = wait_event(&mut rx, Duration::from_secs(10), |ev| {
        matches!(ev, SftpEvent::Listed { .. })
    })
    .expect("列出目录失败");
    let entries = match seen.last().unwrap() {
        SftpEvent::Listed { entries, .. } => entries,
        _ => unreachable!(),
    };
    assert!(
        entries.iter().any(|e| e.name == "Desktop"),
        "家目录应包含 Desktop"
    );

    // ============ 3. 上传 ============
    let local_file = std::env::temp_dir().join("kun_sftp_test_upload.txt");
    std::fs::write(&local_file, b"kun sftp test content\n").unwrap();
    let remote_file = format!("{remote_home}/kun_sftp_test_upload.txt");
    handle.upload(&local_file, &remote_file);
    let _ = wait_event(&mut rx, Duration::from_secs(10), |ev| {
        matches!(ev, SftpEvent::Done { .. } | SftpEvent::Error { .. })
    })
    .expect("上传未完成");

    // ============ 4. 验证远程文件存在 ============
    handle.list(&remote_home);
    let seen = wait_event(&mut rx, Duration::from_secs(10), |ev| {
        matches!(ev, SftpEvent::Listed { .. })
    })
    .expect("重新列出失败");
    let entries = match seen.last().unwrap() {
        SftpEvent::Listed { entries, .. } => entries,
        _ => unreachable!(),
    };
    assert!(
        entries
            .iter()
            .any(|e| e.name == "kun_sftp_test_upload.txt" && e.size == 22),
        "上传文件应存在且大小正确，实际：{entries:?}"
    );

    // ============ 5. 下载 ============
    let local_dl = std::env::temp_dir().join("kun_sftp_test_download.txt");
    handle.download(&remote_file, &local_dl);
    let _ = wait_event(&mut rx, Duration::from_secs(10), |ev| {
        matches!(ev, SftpEvent::Done { .. } | SftpEvent::Error { .. })
    })
    .expect("下载未完成");
    assert_eq!(
        std::fs::read_to_string(&local_dl).unwrap(),
        "kun sftp test content\n",
        "下载内容应与上传一致"
    );

    // ============ 6. 新建目录 + 重命名 ============
    let dir_a = format!("{remote_home}/kun_sftp_test_dir");
    let dir_b = format!("{remote_home}/kun_sftp_test_dir_renamed");
    handle.mkdir(&dir_a);
    let _ = wait_event(&mut rx, Duration::from_secs(10), |ev| {
        matches!(ev, SftpEvent::Done { .. } | SftpEvent::Error { .. })
    })
    .expect("新建目录未完成");
    handle.rename(&dir_a, &dir_b);
    let _ = wait_event(&mut rx, Duration::from_secs(10), |ev| {
        matches!(ev, SftpEvent::Done { .. } | SftpEvent::Error { .. })
    })
    .expect("重命名未完成");

    // ============ 7. 删除（文件 + 目录） ============
    handle.remove(&remote_file, false);
    let _ = wait_event(&mut rx, Duration::from_secs(10), |ev| {
        matches!(ev, SftpEvent::Done { .. } | SftpEvent::Error { .. })
    })
    .expect("删除文件未完成");
    handle.remove(&dir_b, true);
    let _ = wait_event(&mut rx, Duration::from_secs(10), |ev| {
        matches!(ev, SftpEvent::Done { .. } | SftpEvent::Error { .. })
    })
    .expect("删除目录未完成");

    // ============ 8. 验证清理干净 ============
    handle.list(&remote_home);
    let seen = wait_event(&mut rx, Duration::from_secs(10), |ev| {
        matches!(ev, SftpEvent::Listed { .. })
    })
    .expect("最终列出失败");
    let entries = match seen.last().unwrap() {
        SftpEvent::Listed { entries, .. } => entries,
        _ => unreachable!(),
    };
    assert!(
        !entries.iter().any(|e| e.name.starts_with("kun_sftp_test")),
        "测试文件应已清理"
    );

    handle.close();
    std::fs::remove_file(&local_file).ok();
    std::fs::remove_file(&local_dl).ok();
}
