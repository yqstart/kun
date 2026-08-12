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

/// 带修饰键的 CSI 序列：`\x1b[1;{mods}{letter}`。
fn csi_with_mods(letter: u8, mods: Mods) -> Vec<u8> {
    if mods.shift || mods.alt || mods.ctrl {
        format!("\x1b[1;{}{}", mods.csi_modifier(), letter as char).into_bytes()
    } else {
        format!("\x1b[{}", letter as char).into_bytes()
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
        '_' => Some(0x1f),
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
    // Ctrl 组合 → 控制字符。
    if mods.ctrl {
        if let Some(ctrl) = ctrl_char(c) {
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
            // 应用键盘模式（APP_KEYPAD）下 Enter 发送 ESC O M。
            if mode.contains(TermMode::APP_KEYPAD) && !mods.alt {
                Some(b"\x1bOM".to_vec())
            } else {
                Some(vec![b'\r'])
            }
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
            Some(if app { b"\x1bOA".to_vec() } else { csi_with_mods(b'A', mods) })
        }
        Key::Down => {
            let app = mode.contains(TermMode::APP_CURSOR);
            Some(if app { b"\x1bOB".to_vec() } else { csi_with_mods(b'B', mods) })
        }
        Key::Right => {
            let app = mode.contains(TermMode::APP_CURSOR);
            Some(if app { b"\x1bOC".to_vec() } else { csi_with_mods(b'C', mods) })
        }
        Key::Left => {
            let app = mode.contains(TermMode::APP_CURSOR);
            Some(if app { b"\x1bOD".to_vec() } else { csi_with_mods(b'D', mods) })
        }
        Key::Home => {
            let app = mode.contains(TermMode::APP_CURSOR);
            Some(if app { b"\x1bOH".to_vec() } else { csi_with_mods(b'H', mods) })
        }
        Key::End => {
            let app = mode.contains(TermMode::APP_CURSOR);
            Some(if app { b"\x1bOF".to_vec() } else { csi_with_mods(b'F', mods) })
        }
        Key::PageUp => {
            if mods.shift {
                // Shift+PageUp 保留给窗口滚动，不发送。
                None
            } else {
                Some(b"\x1b[5~".to_vec())
            }
        }
        Key::PageDown => {
            if mods.shift {
                None
            } else {
                Some(b"\x1b[6~".to_vec())
            }
        }
        Key::Insert => Some(b"\x1b[2~".to_vec()),
        Key::Delete => Some(b"\x1b[3~".to_vec()),
        // 功能键：F1-F4 用 SS3，F5-F12 用 CSI；带修饰键时用 CSI 修饰形式。
        Key::F(n) => Some(match n {
            1..=4 if mods.shift || mods.alt || mods.ctrl => {
                format!("\x1b[1;{}P", mods.csi_modifier()).into_bytes()
            }
            1 => b"\x1bOP".to_vec(),
            2 => b"\x1bOQ".to_vec(),
            3 => b"\x1bOR".to_vec(),
            4 => b"\x1bOS".to_vec(),
            5 => b"\x1b[15~".to_vec(),
            6 => b"\x1b[17~".to_vec(),
            7 => b"\x1b[18~".to_vec(),
            8 => b"\x1b[19~".to_vec(),
            9 => b"\x1b[20~".to_vec(),
            10 => b"\x1b[21~".to_vec(),
            11 => b"\x1b[23~".to_vec(),
            12 => b"\x1b[24~".to_vec(),
            _ => b"".to_vec(),
        }),
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
        let mods = Mods { ctrl: true, ..Default::default() };
        assert_eq!(encode_key(Key::Char('c'), mods, TermMode::NONE).unwrap(), vec![0x03]);
        assert_eq!(encode_key(Key::Char('a'), mods, TermMode::NONE).unwrap(), vec![0x01]);
    }

    #[test]
    fn alt前缀转义() {
        let mods = Mods { alt: true, ..Default::default() };
        let bytes = encode_key(Key::Char('x'), mods, TermMode::NONE).unwrap();
        assert_eq!(bytes, b"\x1bx");
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
        let mods = Mods { shift: true, ..Default::default() };
        let bytes = encode_key(Key::Up, mods, TermMode::NONE).unwrap();
        assert_eq!(bytes, b"\x1b[1;2A");
        let mods = Mods { ctrl: true, ..Default::default() };
        let bytes = encode_key(Key::Left, mods, TermMode::NONE).unwrap();
        assert_eq!(bytes, b"\x1b[1;5D");
    }

    #[test]
    #[allow(non_snake_case)]
    fn shift_tab_reverse() {
        let mods = Mods { shift: true, ..Default::default() };
        assert_eq!(encode_key(Key::Tab, mods, TermMode::NONE).unwrap(), b"\x1b[Z");
    }

    #[test]
    fn enter应用键盘模式() {
        let bytes = encode_key(Key::Enter, no_mods(), TermMode::APP_KEYPAD).unwrap();
        assert_eq!(bytes, b"\x1bOM");
    }

    #[test]
    fn 功能键序列() {
        assert_eq!(encode_key(Key::F(1), no_mods(), TermMode::NONE).unwrap(), b"\x1bOP");
        assert_eq!(encode_key(Key::F(5), no_mods(), TermMode::NONE).unwrap(), b"\x1b[15~");
    }
}
