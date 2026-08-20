//! 基础补全：命令 / 文件 / 目录候选（Warp 风格输入浮层的数据层）。
//!
//! 仅本地会话启用（远程无本机文件系统对应）。输入行由
//! [`InputModel`] 跟踪（写入 PTY 的字节同步维护），候选按
//! 当前 word 前缀匹配：首个词匹配 PATH 可执行文件，
//! 其余匹配当前目录（含 `~/` 展开）的文件与子目录。

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// 候选条目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// 补全后的完整 word（直接替换输入中的当前 word）。
    pub text: String,
    /// 展示文本（浮层中显示）。
    pub display: String,
    /// 类型（决定浮层中的颜色）。
    pub kind: CandidateKind,
}

/// 候选类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CandidateKind {
    /// 命令（排序最前）。
    Command,
    Dir,
    File,
}

/// PATH 可执行文件索引（进程级懒加载缓存，避免每个终端重复扫描）。
static COMMANDS: OnceLock<Vec<String>> = OnceLock::new();

/// shell 内建命令：Warp 的命令提示不应只认识 PATH 中的可执行文件。
/// 这些命令在 zsh/bash 中最常用，加入索引后 `cd`、`export`、`history`
/// 等输入也能得到有效提示。
const SHELL_BUILTINS: &[&str] = &[
    "alias", "bg", "cd", "command", "dirs", "echo", "eval", "exec", "export", "fc", "fg",
    "history", "jobs", "printf", "pwd", "read", "set", "source", "test", "type", "unalias",
    "unset", "wait", "which",
];

/// 扫描 PATH 下的可执行文件（纯函数，可测试）。
fn commands_from_path(path: &str) -> Vec<String> {
    let mut list: Vec<String> = Vec::new();
    for dir in std::env::split_paths(path) {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                // 只收可执行文件（跳过目录与无执行位的文件）。
                if let Ok(meta) = e.metadata() {
                    if meta.is_file() && meta.permissions().mode() & 0o111 != 0 {
                        if let Some(name) = e.file_name().to_str() {
                            list.push(name.to_string());
                        }
                    }
                }
            }
        }
    }
    list.sort();
    list.dedup();
    list
}

/// 全局命令索引（懒加载）。
pub fn command_index() -> &'static [String] {
    COMMANDS.get_or_init(|| {
        let mut commands = std::env::var("PATH")
            .map(|p| commands_from_path(&p))
            .unwrap_or_default()
            .into_iter()
            .collect::<Vec<_>>();
        commands.extend(SHELL_BUILTINS.iter().map(|name| (*name).to_string()));
        commands.sort();
        commands.dedup();
        commands
    })
}

/// 输入行模型：跟踪用户输入（与写入 PTY 的字节同步）。
pub struct InputModel {
    /// 当前输入行内容。
    pub text: String,
    /// 模型是否可靠：箭头/编辑类按键后失效（无法追踪光标位置），
    /// 回车后恢复。
    pub valid: bool,
    /// 当前工作目录（解析 `cd` 命令追踪，初始为会话启动目录）。
    pub cwd: PathBuf,
}

impl InputModel {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            text: String::new(),
            valid: true,
            cwd,
        }
    }

    /// 追加可见文本（写入 PTY 后同步）。
    pub fn push_text(&mut self, s: &str) {
        if self.valid {
            self.text.push_str(s);
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

    /// 回车：执行当前命令（解析 `cd` 更新 cwd），清空输入。
    pub fn execute(&mut self) {
        if let Some(arg) = self.cd_argument() {
            self.apply_cd(&arg);
        }
        self.text.clear();
        self.valid = true;
    }

    /// 回车：在远程会话中按 POSIX 路径规则追踪常用的 `cd` 命令。
    ///
    /// 远程目录无法用本地文件系统 `canonicalize`，因此只做词法归一化；
    /// 未覆盖的 shell 函数、别名或 `cd -` 仍会保留上一次已知目录。
    pub fn execute_remote(&mut self, home: Option<&Path>) {
        if let Some(arg) = self.cd_argument() {
            self.apply_remote_cd(&arg, home);
        }
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
            .filter(|r| r.is_empty() || r.starts_with(char::is_whitespace))
            .map(|r| r.trim().to_string())
    }

    /// 应用本地 `cd` 参数。
    fn apply_cd(&mut self, arg: &str) {
        let home = std::env::var("HOME").map(PathBuf::from).ok();
        let target = if arg.is_empty() {
            home.clone().unwrap_or_else(|| self.cwd.clone())
        } else if let Some(rest) = arg.strip_prefix("~/") {
            match home {
                Some(h) => h.join(rest),
                None => self.cwd.join(arg),
            }
        } else if arg == "-" {
            // `cd -` 回到上次目录：模型无法追踪，保持当前目录。
            return;
        } else {
            let p = Path::new(arg);
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                self.cwd.join(p)
            }
        };
        if let Ok(canon) = std::fs::canonicalize(&target) {
            self.cwd = canon;
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

    /// 控制键/编辑序列（箭头、Ctrl+U/W 等）：模型失效，禁用补全。
    pub fn invalidate(&mut self) {
        self.valid = false;
    }

    /// 重置（Ctrl+C / 新提示符）。
    pub fn reset(&mut self) {
        self.text.clear();
        self.valid = true;
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

/// 解析输入文本中最后一个 word（按空白分隔），返回（字节偏移, word）。
pub fn last_word(text: &str) -> (usize, &str) {
    let trimmed_end = text.trim_end();
    match trimmed_end.rfind(char::is_whitespace) {
        Some(i) => (i + 1, &trimmed_end[i + 1..]),
        None => (0, trimmed_end),
    }
}

/// 计算当前输入行的补全候选（最多 `limit` 条）。
///
/// `commands` 为命令索引（测试可注入，生产传 `command_index()`）。
pub fn compute_candidates(input: &InputModel, commands: &[String], limit: usize) -> Vec<Candidate> {
    if !input.valid {
        return Vec::new();
    }
    let (word_start, word) = last_word(&input.text);
    if word.is_empty() {
        return Vec::new();
    }
    let is_command_pos = word_start == 0;
    let mut out: Vec<Candidate> = Vec::new();

    if is_command_pos && !word.contains('/') {
        // 命令补全。
        for cmd in commands {
            if cmd.starts_with(word) {
                out.push(Candidate {
                    text: cmd.clone(),
                    display: cmd.clone(),
                    kind: CandidateKind::Command,
                });
                if out.len() >= limit {
                    break;
                }
            }
        }
    } else {
        // 文件/目录补全（含 `~/` 展开与子路径）。
        collect_path_candidates(&mut out, input, word, limit);
    }
    out
}

/// 收集路径候选：word 含 `/` 时在子目录中匹配，否则在当前目录匹配。
fn collect_path_candidates(out: &mut Vec<Candidate>, input: &InputModel, word: &str, limit: usize) {
    // 展开 word 为（基准目录, 前缀, 原始前缀）。
    let (base, prefix, raw_prefix) = if let Some(rest) = word.strip_prefix("~/") {
        let home = std::env::var("HOME").map(PathBuf::from).unwrap_or_default();
        (home, rest.to_string(), "~/".to_string())
    } else if let Some((dir, name)) = word.rfind('/').map(|i| (&word[..i + 1], &word[i + 1..])) {
        let joined = input.cwd.join(dir);
        (joined, name.to_string(), dir.to_string())
    } else {
        (input.cwd.clone(), word.to_string(), String::new())
    };

    let Ok(rd) = std::fs::read_dir(&base) else {
        return;
    };
    let mut matched: Vec<Candidate> = Vec::new();
    for e in rd.flatten() {
        let Ok(meta) = e.metadata() else { continue };
        let Some(name) = e.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if !name.starts_with(&prefix) {
            continue;
        }
        let is_dir = meta.is_dir();
        matched.push(Candidate {
            // 补全文本 = 原始前缀 + 匹配段（目录补 `/` 便于继续补全）。
            text: format!("{raw_prefix}{name}{}", if is_dir { "/" } else { "" }),
            display: name,
            kind: if is_dir {
                CandidateKind::Dir
            } else {
                CandidateKind::File
            },
        });
    }
    // read_dir 顺序不保证，按名称排序（目录优先）。
    matched.sort_by(|a, b| {
        b.kind
            .cmp(&a.kind)
            .then_with(|| a.display.to_lowercase().cmp(&b.display.to_lowercase()))
    });
    out.extend(matched.into_iter().take(limit.saturating_sub(out.len())));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 模型基本编辑() {
        let mut m = InputModel::new(PathBuf::from("/tmp"));
        m.push_text("ls -la");
        assert_eq!(m.text, "ls -la");
        m.backspace();
        assert_eq!(m.text, "ls -l");
        m.execute();
        assert!(m.text.is_empty());
        assert!(m.valid);
        // 中文按字符退格。
        m.push_text("测试");
        m.backspace();
        assert_eq!(m.text, "测");
    }

    #[test]
    fn 模型失效与恢复() {
        let mut m = InputModel::new(PathBuf::from("/tmp"));
        m.push_text("echo hi");
        m.invalidate();
        // 失效后不再更新。
        m.push_text("x");
        m.backspace();
        assert_eq!(m.text, "echo hi");
        m.reset();
        assert!(m.text.is_empty() && m.valid);
    }

    #[test]
    fn cd解析更新工作目录() {
        let tmp = std::env::temp_dir();
        // 临时目录结构：<tmp>/mino-cd-test-<pid>/sub
        let base = tmp.join(format!("mino-cd-test-{}", std::process::id()));
        let sub = base.join("sub");
        std::fs::create_dir_all(&sub).ok();
        let mut m = InputModel::new(tmp.clone());
        m.push_text(&format!("cd {}", base.display()));
        m.execute();
        assert_eq!(m.cwd, std::fs::canonicalize(&base).unwrap());
        // 相对路径。
        m.push_text("cd sub");
        m.execute();
        assert_eq!(m.cwd, std::fs::canonicalize(&sub).unwrap());
        // 无参 cd → HOME。
        let home = std::env::var("HOME").unwrap();
        m.push_text("cd");
        m.execute();
        assert_eq!(m.cwd, PathBuf::from(&home));
        // 非法目录不改变。
        m.push_text("cd /no/such/dir-xyz");
        m.execute();
        assert_eq!(m.cwd, PathBuf::from(&home));
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn cd前缀词不误判() {
        // 回归测试：`cdfoo` 等以 cd 开头的命令曾被误判为 `cd foo`。
        let tmp = std::env::temp_dir();
        let mut m = InputModel::new(tmp.clone());
        m.push_text("cdfoo /tmp");
        m.execute();
        assert_eq!(m.cwd, tmp, "cdfoo 不是 cd 命令，不应改变工作目录");
        // `cd` 后带空白才是 cd 命令（cwd 经 canonicalize，macOS 上
        // /tmp 是 /private/tmp 的符号链接，断言需同样 canonicalize）。
        let mut m2 = InputModel::new(tmp.clone());
        m2.push_text("cd   /tmp");
        m2.execute();
        assert_eq!(
            m2.cwd,
            std::fs::canonicalize("/tmp").unwrap(),
            "cd 带空白参数应生效"
        );
    }

    #[test]
    fn 远程cd按路径规则追踪() {
        let mut m = InputModel::new(PathBuf::from("/srv/app"));
        let home = PathBuf::from("/home/demo");
        m.push_text("cd ../logs");
        m.execute_remote(Some(&home));
        assert_eq!(m.cwd, PathBuf::from("/srv/logs"));

        m.push_text("cd ~/workspace/./mino");
        m.execute_remote(Some(&home));
        assert_eq!(m.cwd, PathBuf::from("/home/demo/workspace/mino"));

        m.push_text("cd ../../../../");
        m.execute_remote(Some(&home));
        assert_eq!(m.cwd, PathBuf::from("/"));
    }

    #[test]
    fn 最后一个词提取() {
        assert_eq!(last_word("ls -la"), (3, "-la"));
        assert_eq!(last_word("cd /tmp"), (3, "/tmp"));
        assert_eq!(last_word("git"), (0, "git"));
        assert_eq!(last_word("echo hi  "), (5, "hi"));
        assert_eq!(last_word(""), (0, ""));
    }

    #[test]
    fn 命令候选() {
        let mut m = InputModel::new(PathBuf::from("/tmp"));
        m.push_text("ca");
        // command_index() 返回排序列表，测试模拟同序。
        let cmds = vec!["cargo".into(), "cat".into(), "ls".into()];
        let c = compute_candidates(&m, &cmds, 8);
        let texts: Vec<&str> = c.iter().map(|x| x.text.as_str()).collect();
        assert_eq!(texts, vec!["cargo", "cat"]);
        assert!(c.iter().all(|x| x.kind == CandidateKind::Command));
    }

    #[test]
    fn 路径候选含目录后缀() {
        let tmp = std::env::temp_dir();
        let base = tmp.join(format!("mino-cp-test-{}", std::process::id()));
        std::fs::create_dir_all(base.join("adir")).ok();
        std::fs::write(base.join("afile.txt"), "x").ok();
        let mut m = InputModel::new(base.clone());
        m.push_text("cat a");
        let c = compute_candidates(&m, &[], 8);
        let mut dirs: Vec<&str> = c
            .iter()
            .filter(|x| x.kind == CandidateKind::Dir)
            .map(|x| x.text.as_str())
            .collect();
        dirs.sort();
        assert_eq!(dirs, vec!["adir/"]);
        assert!(c.iter().any(|x| x.text == "afile.txt"));
        // 子路径补全：`cat adir/` 前缀继续匹配。
        m.push_text("/");
        let c2 = compute_candidates(&m, &[], 8);
        assert!(c2.is_empty() || c2.iter().all(|x| x.kind != CandidateKind::Command));
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn 命令索引可执行过滤() {
        let tmp = std::env::temp_dir();
        let base = tmp.join(format!("mino-cmd-test-{}", std::process::id()));
        std::fs::create_dir_all(&base).ok();
        // 可执行文件 + 不可执行文件 + 目录。
        let exe = base.join("mino-exe");
        std::fs::write(&exe, "#!/bin/sh\n").ok();
        let _ = std::process::Command::new("chmod")
            .arg("+x")
            .arg(&exe)
            .status();
        std::fs::write(base.join("mino-plain"), "x").ok();
        std::fs::create_dir(base.join("mino-dir")).ok();
        let list = commands_from_path(base.to_str().unwrap());
        assert_eq!(list, vec!["mino-exe"]);
        std::fs::remove_dir_all(&base).ok();
    }
}
