//! 终端工作目录跟踪。
//!
//! 终端本身仍由 shell 负责命令编辑和补全；这里仅根据已经写入 PTY 的字节，
//! 尝试追踪常见的 `cd` 命令，为 SFTP 面板提供终端当前目录的快捷定位。

use std::path::{Path, PathBuf};

/// 终端工作目录跟踪器。
pub struct WorkdirTracker {
    text: String,
    valid: bool,
    cwd: PathBuf,
}

impl WorkdirTracker {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            text: String::new(),
            valid: true,
            cwd,
        }
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// 追加可见文本（写入 PTY 后同步）。
    pub fn push_text(&mut self, text: &str) {
        if self.valid {
            self.text.push_str(text);
        }
    }

    /// 退格：删除末尾一个字符。
    pub fn backspace(&mut self) {
        if self.valid {
            self.text.pop();
        }
    }

    /// 覆盖当前工作目录（远程会话由 SFTP 连接建立时提供初始目录）。
    pub fn set_cwd(&mut self, cwd: PathBuf) {
        self.cwd = cwd;
        self.valid = true;
    }

    /// 回车：执行当前命令，尝试解析 `cd` 后清空输入。
    pub fn execute(&mut self) {
        if self.valid {
            if let Some(arg) = self.cd_argument() {
                self.apply_local_cd(&arg);
            }
        }
        self.text.clear();
        self.valid = true;
    }

    /// 回车：在远程会话中按 POSIX 路径规则追踪常用的 `cd` 命令。
    ///
    /// 远程目录无法用本地文件系统 `canonicalize`，因此只做词法归一化；
    /// 未覆盖的 shell 函数、别名或 `cd -` 仍会保留上一次已知目录。
    pub fn execute_remote(&mut self, home: Option<&Path>) {
        if self.valid {
            if let Some(arg) = self.cd_argument() {
                self.apply_remote_cd(&arg, home);
            }
        }
        self.text.clear();
        self.valid = true;
    }

    /// 控制键/编辑序列后无法可靠追踪当前输入，暂停目录更新。
    pub fn invalidate(&mut self) {
        self.valid = false;
    }

    /// 重置当前输入跟踪。
    pub fn reset(&mut self) {
        self.text.clear();
        self.valid = true;
    }

    /// 提取当前输入中的 `cd` 参数。
    fn cd_argument(&self) -> Option<String> {
        // 前缀判定要求 `cd` 后为空或空白开头——`cdfoo` 是另一个命令，
        // 不能误判为 `cd foo`。
        self.text
            .trim()
            .strip_prefix("cd")
            .filter(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
            .map(|rest| rest.trim().to_string())
    }

    /// 应用本地 `cd` 参数。
    fn apply_local_cd(&mut self, arg: &str) {
        let home = std::env::var("HOME").map(PathBuf::from).ok();
        let target = if arg.is_empty() {
            home.clone().unwrap_or_else(|| self.cwd.clone())
        } else if let Some(rest) = arg.strip_prefix("~/") {
            match home {
                Some(home) => home.join(rest),
                None => self.cwd.join(arg),
            }
        } else if arg == "-" {
            // `cd -` 依赖 shell 保存的上一个目录，跟踪器无法可靠推断。
            return;
        } else {
            let path = Path::new(arg);
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                self.cwd.join(path)
            }
        };
        if let Ok(canonical) = std::fs::canonicalize(target) {
            self.cwd = canonical;
        }
    }

    /// 应用远程 `cd` 参数，不访问本地文件系统。
    fn apply_remote_cd(&mut self, arg: &str, home: Option<&Path>) {
        if arg == "-" {
            return;
        }
        let target = if arg.is_empty() || arg == "~" {
            home.unwrap_or(&self.cwd).to_path_buf()
        } else if let Some(rest) = arg.strip_prefix("~/") {
            home.unwrap_or(&self.cwd).join(rest)
        } else {
            let path = Path::new(arg);
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                self.cwd.join(path)
            }
        };
        self.cwd = normalize_remote_path(&target);
    }
}

/// 归一化远程 POSIX 路径，至少保证根目录不会被 `..` 越过。
fn normalize_remote_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::from("/");
    for component in path.components() {
        match component {
            std::path::Component::RootDir | std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
                if normalized.as_os_str().is_empty() {
                    normalized.push("/");
                }
            }
            std::path::Component::Normal(part) => normalized.push(part),
            std::path::Component::Prefix(_) => {}
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cd解析更新工作目录() {
        let tmp = std::env::temp_dir();
        let base = tmp.join(format!("mino-cd-test-{}", std::process::id()));
        let sub = base.join("sub");
        std::fs::create_dir_all(&sub).ok();

        let mut tracker = WorkdirTracker::new(tmp.clone());
        tracker.push_text(&format!("cd {}", base.display()));
        tracker.execute();
        assert_eq!(tracker.cwd, std::fs::canonicalize(&base).unwrap());

        tracker.push_text("cd sub");
        tracker.execute();
        assert_eq!(tracker.cwd, std::fs::canonicalize(&sub).unwrap());

        let home = std::env::var("HOME").unwrap();
        tracker.push_text("cd");
        tracker.execute();
        assert_eq!(tracker.cwd, PathBuf::from(&home));

        tracker.push_text("cd /no/such/dir-xyz");
        tracker.execute();
        assert_eq!(tracker.cwd, PathBuf::from(&home));
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn cd前缀词不误判() {
        let tmp = std::env::temp_dir();
        let mut tracker = WorkdirTracker::new(tmp.clone());
        tracker.push_text("cdfoo /tmp");
        tracker.execute();
        assert_eq!(tracker.cwd, tmp, "cdfoo 不是 cd 命令，不应改变工作目录");

        let mut tracker = WorkdirTracker::new(tmp);
        tracker.push_text("cd   /tmp");
        tracker.execute();
        assert_eq!(
            tracker.cwd,
            std::fs::canonicalize("/tmp").unwrap(),
            "cd 带空白参数应生效"
        );
    }

    #[test]
    fn 远程cd按路径规则追踪() {
        let mut tracker = WorkdirTracker::new(PathBuf::from("/srv/app"));
        let home = PathBuf::from("/home/demo");

        tracker.push_text("cd ../logs");
        tracker.execute_remote(Some(&home));
        assert_eq!(tracker.cwd, PathBuf::from("/srv/logs"));

        tracker.push_text("cd ~/workspace/./mino");
        tracker.execute_remote(Some(&home));
        assert_eq!(tracker.cwd, PathBuf::from("/home/demo/workspace/mino"));

        tracker.push_text("cd ../../../../");
        tracker.execute_remote(Some(&home));
        assert_eq!(tracker.cwd, PathBuf::from("/"));
    }
}
