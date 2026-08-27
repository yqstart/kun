//! 键盘事件 → 终端字节序列编码。
//!
//! 参照 Alacritty 应用层（alacritty/src/input/keyboard.rs）的编码逻辑，
//! 支持控制字符、修饰键、功能键与应用程序光标模式（Application Cursor Mode）。

use super::TermMode;

/// 按键（UI 无关的中立表示，由应用层从 GUI 事件映射而来）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Enter,
    Tab,
    Backspace,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    F(u8),
    Char(char),
}

/// 鼠标滚轮方向（xterm 鼠标协议中的按键 64/65）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseWheelDirection {
    Up,
    Down,
}

/// 修饰键集合。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Mods {
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
    pub super_: bool,
}

impl Mods {
    /// XTerm 修饰键编码：1 + shift + 2*alt + 4*ctrl。
    fn csi_modifier(&self) -> u8 {
        1 + self.shift as u8 + 2 * self.alt as u8 + 4 * self.ctrl as u8
    }
}

fn has_xterm_mods(mods: Mods) -> bool {
    mods.shift || mods.alt || mods.ctrl
}

/// 带修饰键的 CSI 序列：`\x1b[1;{mods}{letter}`。
fn csi_with_mods(letter: u8, mods: Mods) -> Vec<u8> {
    if has_xterm_mods(mods) {
        format!("\x1b[1;{}{}", mods.csi_modifier(), letter as char).into_bytes()
    } else {
        format!("\x1b[{}", letter as char).into_bytes()
    }
}

/// 带修饰键的数字参数 CSI 序列：`\x1b[{code};{mods}~`。
fn csi_tilde_with_mods(code: u8, mods: Mods) -> Vec<u8> {
    if has_xterm_mods(mods) {
        format!("\x1b[{};{}~", code, mods.csi_modifier()).into_bytes()
    } else {
        format!("\x1b[{}~", code).into_bytes()
    }
}

/// 应用光标模式下，无修饰键使用 SS3；有修饰键切换为 xterm CSI 形式。
fn app_cursor_key(letter: u8, mods: Mods) -> Vec<u8> {
    if has_xterm_mods(mods) {
        csi_with_mods(letter, mods)
    } else {
        format!("\x1bO{}", letter as char).into_bytes()
    }
}

/// Ctrl+字符 → 控制字符（参照 Alacritty 的 ctrl 映射）。
fn ctrl_char(c: char) -> Option<u8> {
    match c {
        ' ' => Some(0x00),
        '@' | '`' => Some(0x00),
        'a'..='z' => Some(c as u8 - 0x60),
        'A'..='Z' => Some(c as u8 - 0x40),
        '[' | '{' => Some(0x1b),
        '\\' | '|' => Some(0x1c),
        ']' | '}' => Some(0x1d),
        '^' | '~' => Some(0x1e),
        '_' | '-' => Some(0x1f),
        ',' => Some(0x1c),
        '.' => Some(0x1e),
        '2' => Some(0x00),
        '/' => Some(0x1f),
        '?' => Some(0x7f),
        _ => None,
    }
}

/// 将普通字符编码为字节序列（处理 Ctrl/Alt 修饰）。
fn encode_char(c: char, mods: Mods) -> Option<Vec<u8>> {
    // Alt（无 Ctrl）时前缀 ESC。
    if mods.alt && !mods.ctrl && !mods.super_ {
        let mut out = vec![0x1b];
        let mut buf = [0u8; 4];
        out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        return Some(out);
    }
    // Ctrl 组合 → 控制字符；Alt+Ctrl 保留 Alt 的 ESC 前缀（xterm 语义）。
    if mods.ctrl {
        if let Some(ctrl) = ctrl_char(c) {
            if mods.alt && !mods.super_ {
                return Some(vec![0x1b, ctrl]);
            }
            return Some(vec![ctrl]);
        }
        // 无法映射的控制组合丢弃（避免意外写入）。
        return None;
    }
    // 普通字符直接写 UTF-8。
    let mut buf = [0u8; 4];
    Some(c.encode_utf8(&mut buf).as_bytes().to_vec())
}

/// 将按键编码为要写入 PTY 的字节序列。
///
/// 返回 None 表示该按键不产生输出（如纯修饰键组合）。
pub fn encode_key(key: Key, mods: Mods, mode: TermMode) -> Option<Vec<u8>> {
    match key {
        Key::Char(c) => encode_char(c, mods),
        Key::Enter => {
            // 主键盘 Enter 永远发 CR（`\r`）。应用键盘模式（APP_KEYPAD / DECPAM）
            // 只影响数字小键盘（小键盘 Enter 才是 SS3 `ESC O M`），主键盘 Enter
            // 不受影响——曾把主键盘 Enter 按 APP_KEYPAD 编码成 `\x1bOM`，导致
            // zsh 启用应用键盘模式（TERM=xterm-256color 下 zle 自动开启）后
            // 回车不执行（命令停在命令行）。
            Some(vec![b'\r'])
        }
        Key::Tab => {
            if mods.shift {
                Some(b"\x1b[Z".to_vec())
            } else {
                Some(vec![b'\t'])
            }
        }
        Key::Backspace => Some(vec![0x7f]),
        Key::Escape => Some(vec![0x1b]),
        // 方向键：应用光标模式下用 SS3 编码（ESC O x）。
        Key::Up => {
            let app = mode.contains(TermMode::APP_CURSOR);
            Some(if app {
                app_cursor_key(b'A', mods)
            } else {
                csi_with_mods(b'A', mods)
            })
        }
        Key::Down => {
            let app = mode.contains(TermMode::APP_CURSOR);
            Some(if app {
                app_cursor_key(b'B', mods)
            } else {
                csi_with_mods(b'B', mods)
            })
        }
        Key::Right => {
            let app = mode.contains(TermMode::APP_CURSOR);
            Some(if app {
                app_cursor_key(b'C', mods)
            } else {
                csi_with_mods(b'C', mods)
            })
        }
        Key::Left => {
            let app = mode.contains(TermMode::APP_CURSOR);
            Some(if app {
                app_cursor_key(b'D', mods)
            } else {
                csi_with_mods(b'D', mods)
            })
        }
        Key::Home => {
            let app = mode.contains(TermMode::APP_CURSOR);
            Some(if app {
                app_cursor_key(b'H', mods)
            } else {
                csi_with_mods(b'H', mods)
            })
        }
        Key::End => {
            let app = mode.contains(TermMode::APP_CURSOR);
            Some(if app {
                app_cursor_key(b'F', mods)
            } else {
                csi_with_mods(b'F', mods)
            })
        }
        Key::PageUp => {
            if mods.shift && !mods.alt && !mods.ctrl {
                // Shift+PageUp 保留给窗口滚动，不发送。
                None
            } else {
                Some(csi_tilde_with_mods(5, mods))
            }
        }
        Key::PageDown => {
            if mods.shift && !mods.alt && !mods.ctrl {
                None
            } else {
                Some(csi_tilde_with_mods(6, mods))
            }
        }
        Key::Insert => Some(csi_tilde_with_mods(2, mods)),
        Key::Delete => Some(csi_tilde_with_mods(3, mods)),
        // 功能键：F1-F4 用 SS3，F5-F12 用 CSI；带修饰键时用 CSI 修饰形式。
        // 修饰形式末尾字母按 xterm 规范随键递增（P/Q/R/S = F1-F4）——
        // 曾固定发 'P'，导致 F2-F4 带修饰键时被终端识别为 F1。
        // egui 可能提供 F13-F35，但本编码器没有可靠的 xterm 映射；返回
        // None，避免把“不支持的按键”伪装成一次空写入。
        Key::F(n) => match n {
            1..=4 if has_xterm_mods(mods) => Some(
                format!("\x1b[1;{}{}", mods.csi_modifier(), (b'P' + (n - 1)) as char).into_bytes(),
            ),
            1 => Some(b"\x1bOP".to_vec()),
            2 => Some(b"\x1bOQ".to_vec()),
            3 => Some(b"\x1bOR".to_vec()),
            4 => Some(b"\x1bOS".to_vec()),
            5 => Some(csi_tilde_with_mods(15, mods)),
            6 => Some(csi_tilde_with_mods(17, mods)),
            7 => Some(csi_tilde_with_mods(18, mods)),
            8 => Some(csi_tilde_with_mods(19, mods)),
            9 => Some(csi_tilde_with_mods(20, mods)),
            10 => Some(csi_tilde_with_mods(21, mods)),
            11 => Some(csi_tilde_with_mods(23, mods)),
            12 => Some(csi_tilde_with_mods(24, mods)),
            _ => None,
        },
    }
}

/// 将鼠标滚轮事件编码为 xterm 鼠标上报序列。
///
/// `column` 与 `row` 为从零开始的当前视口 cell 坐标。只有终端程序已通过
/// DECSET 启用鼠标上报时才返回字节；优先使用 SGR（1006），并兼容 X10 与
/// UTF-8（1005）编码。
pub fn encode_mouse_wheel(
    direction: MouseWheelDirection,
    mods: Mods,
    mode: TermMode,
    column: usize,
    row: usize,
) -> Option<Vec<u8>> {
    if !mode.intersects(TermMode::MOUSE_MODE) {
        return None;
    }

    let button = match direction {
        MouseWheelDirection::Up => 64,
        MouseWheelDirection::Down => 65,
    } + mouse_modifier_bits(mods);
    let column = column.saturating_add(1);
    let row = row.saturating_add(1);

    if mode.contains(TermMode::SGR_MOUSE) {
        return Some(format!("\x1b[<{button};{column};{row}M").into_bytes());
    }

    let mut output = b"\x1b[M".to_vec();
    let utf8 = mode.contains(TermMode::UTF8_MOUSE);
    push_legacy_mouse_component(&mut output, button, utf8);
    push_legacy_mouse_component(&mut output, column, utf8);
    push_legacy_mouse_component(&mut output, row, utf8);
    Some(output)
}

/// xterm 鼠标修饰位：Shift=4、Alt=8、Ctrl=16。
fn mouse_modifier_bits(mods: Mods) -> usize {
    (mods.shift as usize * 4) + (mods.alt as usize * 8) + (mods.ctrl as usize * 16)
}

/// 追加传统 X10/UTF-8 鼠标协议中的一个 `value + 32` 分量。
fn push_legacy_mouse_component(output: &mut Vec<u8>, value: usize, utf8: bool) {
    let value = value.saturating_add(32);
    if utf8 {
        // xterm 1005 使用最多两字节 UTF-8 坐标，最大可表达 U+07FF。
        let value = value.min(0x07ff) as u32;
        let character = char::from_u32(value).expect("U+07FF 范围内始终是有效标量值");
        let mut buffer = [0; 4];
        output.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
    } else {
        // 传统 X10 每个分量只有一个字节；超出范围时按 xterm 约定钳制。
        output.push(value.min(u8::MAX as usize) as u8);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_mods() -> Mods {
        Mods::default()
    }

    #[test]
    fn 普通字符直接输出() {
        let bytes = encode_key(Key::Char('a'), no_mods(), TermMode::NONE).unwrap();
        assert_eq!(bytes, b"a");
    }

    #[test]
    fn utf8中文编码() {
        let bytes = encode_key(Key::Char('中'), no_mods(), TermMode::NONE).unwrap();
        assert_eq!(bytes, "中".as_bytes());
    }

    #[test]
    fn ctrl字母映射控制字符() {
        let mods = Mods {
            ctrl: true,
            ..Default::default()
        };
        assert_eq!(
            encode_key(Key::Char('c'), mods, TermMode::NONE).unwrap(),
            vec![0x03]
        );
        assert_eq!(
            encode_key(Key::Char('a'), mods, TermMode::NONE).unwrap(),
            vec![0x01]
        );
    }

    #[test]
    fn ctrl标点映射控制字符() {
        let mods = Mods {
            ctrl: true,
            ..Default::default()
        };
        assert_eq!(
            encode_key(Key::Char('-'), mods, TermMode::NONE).unwrap(),
            vec![0x1f]
        );
        assert_eq!(
            encode_key(Key::Char(','), mods, TermMode::NONE).unwrap(),
            vec![0x1c]
        );
        assert_eq!(
            encode_key(Key::Char('.'), mods, TermMode::NONE).unwrap(),
            vec![0x1e]
        );
        assert_eq!(
            encode_key(Key::Char('2'), mods, TermMode::NONE).unwrap(),
            vec![0x00]
        );
    }

    #[test]
    fn alt前缀转义() {
        let mods = Mods {
            alt: true,
            ..Default::default()
        };
        let bytes = encode_key(Key::Char('x'), mods, TermMode::NONE).unwrap();
        assert_eq!(bytes, b"\x1bx");
    }

    #[test]
    fn alt_ctrl保留转义前缀() {
        let mods = Mods {
            alt: true,
            ctrl: true,
            ..Default::default()
        };
        assert_eq!(
            encode_key(Key::Char('c'), mods, TermMode::NONE).unwrap(),
            b"\x1b\x03"
        );
        assert_eq!(
            encode_key(Key::Char('m'), mods, TermMode::NONE).unwrap(),
            b"\x1b\r"
        );
    }

    #[test]
    fn 方向键普通与应用模式() {
        let normal = encode_key(Key::Up, no_mods(), TermMode::NONE).unwrap();
        assert_eq!(normal, b"\x1b[A");
        let app = encode_key(Key::Up, no_mods(), TermMode::APP_CURSOR).unwrap();
        assert_eq!(app, b"\x1bOA");
    }

    #[test]
    fn 方向键带修饰符() {
        let mods = Mods {
            shift: true,
            ..Default::default()
        };
        let bytes = encode_key(Key::Up, mods, TermMode::NONE).unwrap();
        assert_eq!(bytes, b"\x1b[1;2A");
        let mods = Mods {
            ctrl: true,
            ..Default::default()
        };
        let bytes = encode_key(Key::Left, mods, TermMode::NONE).unwrap();
        assert_eq!(bytes, b"\x1b[1;5D");
    }

    #[test]
    fn 应用光标模式保留方向键修饰符() {
        let shift = Mods {
            shift: true,
            ..Default::default()
        };
        assert_eq!(
            encode_key(Key::Up, shift, TermMode::APP_CURSOR).unwrap(),
            b"\x1b[1;2A"
        );

        let ctrl = Mods {
            ctrl: true,
            ..Default::default()
        };
        assert_eq!(
            encode_key(Key::Home, ctrl, TermMode::APP_CURSOR).unwrap(),
            b"\x1b[1;5H"
        );
    }

    #[test]
    #[allow(non_snake_case)]
    fn shift_tab_reverse() {
        let mods = Mods {
            shift: true,
            ..Default::default()
        };
        assert_eq!(
            encode_key(Key::Tab, mods, TermMode::NONE).unwrap(),
            b"\x1b[Z"
        );
    }

    #[test]
    fn enter应用键盘模式() {
        // 主键盘 Enter 不受应用键盘模式影响，永远发 CR（\r）。
        // 曾误编码为 \x1bOM（数字小键盘 Enter 序列），导致 zsh 启用
        // 应用键盘模式后回车不执行（回归测试）。
        let bytes = encode_key(Key::Enter, no_mods(), TermMode::APP_KEYPAD).unwrap();
        assert_eq!(bytes, b"\r");
    }

    #[test]
    fn 鼠标滚轮按xterm协议编码() {
        let sgr_mode = TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE;
        assert_eq!(
            encode_mouse_wheel(MouseWheelDirection::Up, no_mods(), sgr_mode, 2, 4).unwrap(),
            b"\x1b[<64;3;5M"
        );
        assert_eq!(
            encode_mouse_wheel(
                MouseWheelDirection::Up,
                Mods {
                    ctrl: true,
                    ..Default::default()
                },
                sgr_mode,
                2,
                4,
            )
            .unwrap(),
            b"\x1b[<80;3;5M"
        );

        let x10 = encode_mouse_wheel(
            MouseWheelDirection::Up,
            no_mods(),
            TermMode::MOUSE_REPORT_CLICK,
            2,
            4,
        )
        .unwrap();
        assert_eq!(x10, vec![0x1b, b'[', b'M', 96, 35, 37]);
    }

    #[test]
    fn 鼠标滚轮utf8坐标编码() {
        // column=223 → 1-based 坐标 224，传统单字节协议无法表达；1005 应编码 U+0100。
        let bytes = encode_mouse_wheel(
            MouseWheelDirection::Up,
            no_mods(),
            TermMode::MOUSE_REPORT_CLICK | TermMode::UTF8_MOUSE,
            223,
            4,
        )
        .unwrap();
        assert_eq!(bytes, vec![0x1b, b'[', b'M', 96, 0xc4, 0x80, 37]);
    }

    #[test]
    fn 鼠标滚轮未启用上报时不编码() {
        assert!(
            encode_mouse_wheel(MouseWheelDirection::Down, no_mods(), TermMode::NONE, 0, 0,)
                .is_none()
        );
    }

    #[test]
    fn 功能键序列() {
        assert_eq!(
            encode_key(Key::F(1), no_mods(), TermMode::NONE).unwrap(),
            b"\x1bOP"
        );
        assert_eq!(
            encode_key(Key::F(5), no_mods(), TermMode::NONE).unwrap(),
            b"\x1b[15~"
        );
        assert!(encode_key(Key::F(13), no_mods(), TermMode::NONE).is_none());
    }

    #[test]
    fn 功能键带修饰键按xterm规范编码() {
        // 回归测试：F2-F4 带修饰键曾固定发 'P'（F1 的末尾字母），
        // 终端把 F2-F4 全识别为 F1。xterm 规范末尾字母随键递增。
        let shift = Mods {
            shift: true,
            ..Default::default()
        };
        assert_eq!(
            encode_key(Key::F(1), shift, TermMode::NONE).unwrap(),
            b"\x1b[1;2P"
        );
        assert_eq!(
            encode_key(Key::F(2), shift, TermMode::NONE).unwrap(),
            b"\x1b[1;2Q"
        );
        assert_eq!(
            encode_key(Key::F(3), shift, TermMode::NONE).unwrap(),
            b"\x1b[1;2R"
        );
        let ctrl = Mods {
            ctrl: true,
            ..Default::default()
        };
        assert_eq!(
            encode_key(Key::F(4), ctrl, TermMode::NONE).unwrap(),
            b"\x1b[1;5S"
        );

        assert_eq!(
            encode_key(Key::F(5), shift, TermMode::NONE).unwrap(),
            b"\x1b[15;2~"
        );
        assert_eq!(
            encode_key(Key::F(12), ctrl, TermMode::NONE).unwrap(),
            b"\x1b[24;5~"
        );
    }

    #[test]
    fn 编辑与翻页键带修饰键按xterm规范编码() {
        let alt = Mods {
            alt: true,
            ..Default::default()
        };
        assert_eq!(
            encode_key(Key::Insert, alt, TermMode::NONE).unwrap(),
            b"\x1b[2;3~"
        );
        assert_eq!(
            encode_key(Key::PageUp, alt, TermMode::NONE).unwrap(),
            b"\x1b[5;3~"
        );
        assert_eq!(
            encode_key(
                Key::Delete,
                Mods {
                    ctrl: true,
                    ..Default::default()
                },
                TermMode::NONE
            )
            .unwrap(),
            b"\x1b[3;5~"
        );
        assert!(encode_key(
            Key::PageDown,
            Mods {
                shift: true,
                ..Default::default()
            },
            TermMode::NONE
        )
        .is_none());
    }
}
