use std::io;
use thiserror::Error;

/// ftool 项目的自定义错误类型
#[derive(Debug, Error)]
pub enum FtoolError {
    /// IO 操作错误，自动包装 `std::io::Error`
    #[error("IO 错误: {0}")]
    Io(#[from] io::Error),
    /// 权限不足（需要 root 等更高权限，通常提示改用 sudo 执行）
    #[error("权限错误: {0}")]
    Permission(String),

    /// 文件操作失败（打开/读取/写入文件失败）
    #[error("文件错误: {0}")]
    File(String),
    /// 内核签名相关错误
    #[error("签名错误: {0}")]
    Sign(String),

    /// 系统升级相关错误
    #[error("升级错误: {0}")]
    Upgrade(String),

    /// 用户输入无效
    #[error("输入无效: {0}")]
    Input(String),

    /// 进程执行相关错误
    #[error("执行错误: {0}")]
    Process(String),

    /// 显卡模式切换相关错误
    #[error("显卡切换错误: {0}")]
    Gpu(String),
}
