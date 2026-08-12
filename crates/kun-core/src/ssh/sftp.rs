//! SFTP 客户端（基于 russh-sftp）。M3 里程碑实现。

/// 远程文件条目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<u64>,
    pub permissions: u32,
}
