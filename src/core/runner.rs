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
    pub fn run_with_timeout<I, S>(
        cmd: &str,
        args: I,
        timeout_secs: u64,
    ) -> Result<Output, FtoolError>
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

        // 用独立线程持续排空 stdout/stderr：若子进程输出超过管道缓冲
        // （默认约 64KiB）而无人读取，子进程会阻塞在 write 上无法退出，
        // 即使运行正常也会被误判为超时。读线程在管道写端关闭
        // （进程退出或被杀）后读到 EOF 自然结束。
        let stdout_pipe = child.stdout.take();
        let stderr_pipe = child.stderr.take();
        let stdout_thread = std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut pipe) = stdout_pipe {
                let _ = pipe.read_to_end(&mut buf);
            }
            buf
        });
        let stderr_thread = std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut pipe) = stderr_pipe {
                let _ = pipe.read_to_end(&mut buf);
            }
            buf
        });

        let start = std::time::Instant::now();
        let timeout = Duration::from_secs(timeout_secs);

        let mut status: Option<ExitStatus> = None;
        let mut timed_out = false;
        let mut wait_error: Option<std::io::Error> = None;
        loop {
            match child.try_wait() {
                Ok(Some(s)) => {
                    status = Some(s);
                    break;
                }
                Ok(None) => {
                    if start.elapsed() >= timeout {
                        let _ = child.kill();
                        timed_out = true;
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(e) => {
                    // try_wait 失败（极罕见，如 waitpid 被信号打断）：尽力终止并
                    // 回收子进程，记下错误退出循环——直接 return 会跳过下方两个
                    // 读管道线程的 join，使排空输出的线程脱离管理
                    let _ = child.kill();
                    let _ = child.wait();
                    wait_error = Some(e);
                    break;
                }
            }
        }

        // 回收子进程；读线程随后读到 EOF 退出（子进程退出即关闭管道写端）
        let _ = child.wait();
        let stdout = stdout_thread.join().unwrap_or_default();
        let stderr = stderr_thread.join().unwrap_or_default();

        // try_wait 失败分支：子进程已在循环内 kill + wait 回收，两个读管道线程
        // 也已 join 完毕，此时才返回错误
        if let Some(e) = wait_error {
            return Err(FtoolError::Io(e));
        }

        if timed_out {
            // 超时错误附上已捕获输出的 stderr 尾部（复用 run_checked 的截断思路，
            // 上限 512 字符），便于定位卡住原因；此时 join 已完成，内容完整
            const MAX_ERR_LEN: usize = 512;
            let stderr_text = String::from_utf8_lossy(&stderr).trim().to_string();
            let detail = if stderr_text.is_empty() {
                String::new()
            } else {
                // 超时原因通常在输出末尾，故保留尾部；超长时按 UTF-8 字符边界截断
                let tail = if stderr_text.len() <= MAX_ERR_LEN {
                    stderr_text
                } else {
                    let mut start = stderr_text.len() - MAX_ERR_LEN;
                    while !stderr_text.is_char_boundary(start) {
                        start += 1;
                    }
                    format!("…{}", &stderr_text[start..])
                };
                format!("，stderr 末尾: {}", tail)
            };
            return Err(FtoolError::Process(format!(
                "命令 '{}' 执行超时 ({}s)，已终止{}",
                cmd, timeout_secs, detail
            )));
        }

        // 不变量：循环内已处理 try_wait 的 Ok(None)/Ok(Some)/Err 三分支——
        // 超时与 try_wait 失败均在上方返回错误，能走到此处 status 必为 Some
        let status = match status {
            Some(s) => s,
            None => unreachable!("子进程状态已获取"),
        };
        Ok(Output {
            status,
            stdout,
            stderr,
        })
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

#[cfg(test)]
mod tests {
    use super::*;

    // ---------- run_with_timeout：超时错误携带已捕获输出 ----------

    /// WHY: 超时错误此前只报告"已终止"、不携带任何已捕获输出，脚本无法判断
    /// 命令是卡死还是输出异常；超时后应把 stderr 尾部内容附在错误消息里
    /// （在 join 完成之后取数，保证 stderr 内容完整）
    #[test]
    fn run_with_timeout_attaches_stderr_tail_to_error() {
        // 向 stderr 写远超 512 字符的诊断输出后挂起，等待被超时终止
        let script = "n=0; while [ $n -lt 300 ]; do echo \"diag line $n 0123456789 0123456789 0123456789\" >&2; n=$((n+1)); done; echo FINAL-TAIL-MARKER >&2; while :; do :; done";
        let err = CommandRunner::run_with_timeout("sh", ["-c", script], 1).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("执行超时"), "{msg}");
        assert!(msg.contains("stderr 末尾"), "错误应附带 stderr 内容: {msg}");
        assert!(
            msg.contains("FINAL-TAIL-MARKER"),
            "应保留 stderr 尾部内容: {msg}"
        );
        assert!(msg.contains("…"), "超过 512 字符时应截断并标注省略: {msg}");
    }
}
