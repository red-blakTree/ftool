use std::io::{self, BufRead, IsTerminal, Write};

/// 交互式提示工具
pub struct Prompter;

impl Prompter {
    /// 向用户显示提示信息并读取输入行
    ///
    /// # 返回
    /// 用户输入的内容（去除首尾空白），空字符串表示用户直接按了回车
    pub fn ask_input(prompt: &str) -> String {
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
    pub fn is_terminal() -> bool {
        io::stdin().is_terminal()
    }
}
