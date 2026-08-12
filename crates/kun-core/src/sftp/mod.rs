//! SSH 连接与 SFTP 文件传输（基于 russh / russh-sftp）。
//! M3 里程碑实现。

/// 远程文件条目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<u64>,
    pub permissions: u32,
}
