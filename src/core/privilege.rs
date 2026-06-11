use crate::core::error::FtoolError;

/// 进程权限检查工具
pub struct Privilege;

impl Privilege {
    /// 确保当前进程以 root 用户运行
    ///
    /// 在 Linux 上检查有效用户 ID（euid）是否为 0。
    /// 非 Linux 系统直接返回错误。
    #[cfg(target_os = "linux")]
    pub fn ensure_root() -> Result<(), FtoolError> {
        if unsafe { libc::geteuid() } == 0 {
            Ok(())
        } else {
            Err(FtoolError::Process(
                "此操作需要 root 权限，请使用 sudo 执行".into(),
            ))
        }
    }

    /// 非 Linux 系统不支持此工具
    #[cfg(not(target_os = "linux"))]
    pub fn ensure_root() -> Result<(), FtoolError> {
        Err(FtoolError::Process("此操作仅支持 Linux 系统".into()))
    }
}
