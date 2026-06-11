use crate::core::error::FtoolError;
use std::ffi::OsStr;
use std::io::Read;
use std::process::{Command, ExitStatus, Output};
use std::time::Duration;

/// 系统命令执行工具
///
/// 封装了 `std::process::Command` 的常见操作模式，
/// 包括简单执行、检查性执行以及状态码校验。
pub struct CommandRunner;

impl CommandRunner {
    /// 执行命令并捕获输出（stdout + stderr）
    pub fn run<I, S>(cmd: &str, args: I) -> Result<Output, FtoolError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Command::new(cmd)
            .args(args)
            .output()
            .map_err(FtoolError::Io)
    }

    /// 执行命令并捕获输出，超出指定时间后终止子进程并返回超时错误
    ///
    /// 适用于 nvidia-smi 等可能因驱动异常而永久阻塞的命令。
    /// 轮询间隔为 100ms，超时后会 kill 子进程避免残留。
    pub fn run_with_timeout<I, S>(cmd: &str, args: I, timeout_secs: u64) -> Result<Output, FtoolError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut child = Command::new(cmd)
            .args(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(FtoolError::Io)?;

        let start = std::time::Instant::now();
        let timeout = Duration::from_secs(timeout_secs);

        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let mut stdout = Vec::new();
                    let mut stderr = Vec::new();
                    if let Some(mut s) = child.stdout.take() {
                        let _ = s.read_to_end(&mut stdout);
                    }
                    if let Some(mut s) = child.stderr.take() {
                        let _ = s.read_to_end(&mut stderr);
                    }
                    return Ok(Output { status, stdout, stderr });
                }
                Ok(None) => {
                    if start.elapsed() >= timeout {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(FtoolError::Process(format!(
                            "命令 '{}' 执行超时 ({}s)",
                            cmd, timeout_secs
                        )));
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(e) => return Err(FtoolError::Io(e)),
            }
        }
    }

    /// 执行命令，仅关心退出状态码（不捕获输出）
    pub fn run_status<I, S>(cmd: &str, args: I) -> Result<ExitStatus, FtoolError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Command::new(cmd)
            .args(args)
            .status()
            .map_err(FtoolError::Io)
    }

    /// 执行命令，失败时自动从 stderr/stdout 提取错误信息
    ///
    /// 优先使用 stderr 内容作为错误信息；若 stderr 为空则回退到 stdout；
    /// 两者皆为空时返回退出码。
    /// 错误信息会被截断到最大 4096 字节以防止输出污染。
    pub fn run_checked<I, S>(cmd: &str, args: I) -> Result<Output, FtoolError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = Self::run(cmd, args)?;
        if output.status.success() {
            return Ok(output);
        }

        const MAX_ERR_LEN: usize = 4096;
        let truncate = |s: &str| -> String {
            if s.len() <= MAX_ERR_LEN {
                s.to_string()
            } else {
                // 找到安全的截断边界，避免切在多字节字符中间
                let mut end = MAX_ERR_LEN;
                while !s.is_char_boundary(end) {
                    end -= 1;
                }
                let mut truncated = s[..end].to_string();
                truncated.push_str("... (truncated)");
                truncated
            }
        };

        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if !stderr.is_empty() {
            return Err(FtoolError::Process(format!(
                "命令 '{}' 执行失败: {}",
                cmd,
                truncate(&stderr)
            )));
        }
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !stdout.is_empty() {
            return Err(FtoolError::Process(format!(
                "命令 '{}' 执行失败: {}",
                cmd,
                truncate(&stdout)
            )));
        }
        Err(FtoolError::Process(format!(
            "命令 '{}' 执行失败，退出码: {}",
            cmd,
            output.status.code().unwrap_or(-1)
        )))
    }

    /// 校验进程退出码，非成功时返回错误
    pub fn ensure_success(status: ExitStatus) -> Result<(), FtoolError> {
        if status.success() {
            return Ok(());
        }
        let msg = status
            .code()
            .map(|code| format!("命令执行失败，退出码: {code}"))
            .unwrap_or_else(|| "命令执行失败：被信号异常终止".to_string());
        Err(FtoolError::Process(msg))
    }
}
