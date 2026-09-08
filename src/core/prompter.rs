use std::io::{self, BufRead, IsTerminal, Write};

/// 非终端（stdin 非交互，如脚本/管道/cron）场景下确认问题的降级策略
///
/// 同一段确认逻辑在终端与非终端下的合理行为可能不同：
/// - 危险/不可逆操作（自动安装、强制门禁）应默认拒绝，避免脚本静默执行；
/// - 幂等/可跳过流程（覆盖确认等）应默认放行，避免脚本流程被卡住。
///
/// 策略在调用点显式声明，消除散落各处的 `is_terminal()` 组合判断。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NonTerminal {
    /// 视同用户未确认（返回 `false`），安全默认
    Deny,
    /// 视同用户已确认（返回 `true`）
    Allow,
}

/// 交互式提示工具
pub struct Prompter;

impl Prompter {
    /// 向用户显示提示信息并读取输入行
    ///
    /// # 返回
    /// 用户输入的内容（去除首尾空白），空字符串表示用户直接按了回车
    pub fn ask_input(prompt: &str) -> String {
        // 设计取舍：提示写入失败（stdout 已关闭/重定向到不可写目标）时按空输入
        // 继续——后续 ask_yes 会落到 default=false 的安全方向，不会因提示不可见
        // 而误放行。SIGPIPE 下 println! 默认 panic 属既有行为，不做全局 SIGPIPE
        // 处理（改动面大），此处保持一致
        let stdout = io::stdout();
        let mut stdout_lock = stdout.lock();
        let _ = write!(stdout_lock, "{prompt}");
        let _ = stdout_lock.flush();

        let stdin = io::stdin();
        let mut buf = String::new();
        let mut stdin_lock = stdin.lock();
        if stdin_lock.read_line(&mut buf).is_err() {
            return String::new();
        }
        buf.trim().to_owned()
    }

    /// 询问 yes/no 确认，支持默认值
    ///
    /// 当用户直接回车时返回 `default`。
    /// 大小写不敏感匹配 "y" 或 "yes" 时返回 `true`，其余返回 `false`。
    pub fn ask_yes(prompt: &str, default: bool) -> bool {
        let input = Self::ask_input(prompt);
        if input.is_empty() {
            return default;
        }
        input.eq_ignore_ascii_case("y") || input.eq_ignore_ascii_case("yes")
    }

    /// 判断标准输入是否为终端（而非管道重定向）
    ///
    /// 已知局限：提示文本写入 stdout，而此处仅检测 stdin——stdout 被重定向但
    /// stdin 仍是终端时，提示对用户不可见却仍在等待其作答；更完善的做法是
    /// 直接读写 /dev/tty，属后续增强，当前保持简单
    pub fn is_terminal() -> bool {
        io::stdin().is_terminal()
    }

    /// 终端交互下询问 yes/no 确认；非终端下按 [`NonTerminal`] 策略直接降级。
    ///
    /// 终端行为与 [`Self::ask_yes`] 一致（回车返回 `default`）。
    pub fn confirm(prompt: &str, default: bool, non_terminal: NonTerminal) -> bool {
        if !Self::is_terminal() {
            return non_terminal == NonTerminal::Allow;
        }
        Self::ask_yes(prompt, default)
    }
}
