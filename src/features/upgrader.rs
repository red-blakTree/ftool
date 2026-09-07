use crate::core::FtoolError;
use crate::core::privilege::Privilege;
use crate::core::prompter::{NonTerminal, Prompter};
use crate::core::runner::CommandRunner;
use std::ffi::OsStr;

/// Fedora 系统版本升级工具
pub struct Upgrader;

impl Upgrader {
    /// 从 /etc/fedora-release 内容解析主版本号（纯函数，便于测试）。
    ///
    /// 典型内容形如 `Fedora release 40 (Forty)`，取 "release" 关键字后的首个数字。
    fn release_from_fedora_release(content: &str) -> Option<u32> {
        let pos = content.find("release")?;
        let rest = &content[pos + 7..];
        rest.split_whitespace().find_map(|word| word.parse().ok())
    }

    /// 从 /etc/os-release 内容解析 VERSION_ID（纯函数，便于测试）。
    fn release_from_os_release(content: &str) -> Option<u32> {
        content.lines().find_map(|line| {
            line.strip_prefix("VERSION_ID=")
                .and_then(|v| v.trim_matches('"').parse().ok())
        })
    }

    /// 检测当前 Fedora 主版本号
    ///
    /// 优先从 /etc/fedora-release 解析，降级到 /etc/os-release。
    fn fedora_version() -> Result<u32, FtoolError> {
        if let Ok(content) = std::fs::read_to_string("/etc/fedora-release")
            && let Some(v) = Self::release_from_fedora_release(&content)
        {
            return Ok(v);
        }
        if let Ok(content) = std::fs::read_to_string("/etc/os-release")
            && let Some(v) = Self::release_from_os_release(&content)
        {
            return Ok(v);
        }
        Err(FtoolError::Upgrade("无法检测 Fedora 版本".into()))
    }

    /// 执行 Fedora 系统大版本升级（如 Fedora 40 → 41）
    ///
    /// 流程编排：人工确认 → 可用性检测 → 可选更新当前系统 → 能力探测 →
    /// 下载软件包 → 触发离线重启（或给出手动命令）。
    pub fn perform_upgrade() -> Result<(), FtoolError> {
        let cur = Self::fedora_version()?;
        let next = cur + 1;
        println!("\n⚠️ 即将进行系统升级: Fedora {cur} → {next}");

        Self::confirm_upgrade()?;
        Self::ensure_release_available(next)?;
        Self::maybe_update_current()?;
        Self::ensure_system_upgrade_supported()?;
        Self::download_packages(next)?;
        Self::reboot_or_hint()?;
        Ok(())
    }

    /// 第一步确认：升级是不可逆的大动作，非交互终端（管道/脚本/cron）下禁止
    /// 自动放行，必须由人工在终端确认后才继续
    fn confirm_upgrade() -> Result<(), FtoolError> {
        if !Prompter::is_terminal() {
            return Err(FtoolError::Upgrade(
                "stdin 不是交互终端，已中止升级（升级操作必须人工在终端确认）".into(),
            ));
        }
        if !Prompter::ask_yes("是否继续？ [y/N]: ", false) {
            println!("已取消升级。");
            return Ok(());
        }
        Ok(())
    }

    /// [1/3] 检测目标版本软件源可用性（dnf check-update 退出码 0/100 表示可用）
    fn ensure_release_available(next: u32) -> Result<(), FtoolError> {
        println!("\n[1/3] 检测 Fedora {next} 可用性...");
        let ver = next.to_string();

        // 使用 run 获取输出，失败时透传 stderr
        let output = CommandRunner::run("dnf", ["check-update", "--releasever", &ver])?;
        match output.status.code() {
            Some(0) | Some(100) => println!("✅ Fedora {next} 可用"),
            _ => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(FtoolError::Upgrade(format!(
                    "Fedora {next} 可能尚未发布或源配置错误:\n{}",
                    stderr.trim()
                )));
            }
        }
        Ok(())
    }

    /// 可选：先更新当前系统到最新状态
    fn maybe_update_current() -> Result<(), FtoolError> {
        if Prompter::confirm("是否先更新当前系统？ [y/N]: ", false, NonTerminal::Deny) {
            println!("\n📦 更新当前系统...");
            let status = CommandRunner::run_status("dnf", ["upgrade", "--refresh"])?;
            CommandRunner::ensure_success(status)?;
        }
        Ok(())
    }

    /// 检查 dnf 是否支持 system-upgrade（能力探测：--help 无副作用）
    ///
    /// DNF4 需要 dnf-plugin-system-upgrade；DNF5 在 Fedora 42+ 已内建，
    /// 更早版本需要 dnf5-plugin-system-upgrade——插件包名随 dnf 世代而变，
    /// 不能硬编码包名，故直接探测子命令是否可用。
    fn ensure_system_upgrade_supported() -> Result<(), FtoolError> {
        if !Self::supports_system_upgrade() {
            let pkg = if Self::dnf_is_dnf5() {
                "dnf5-plugin-system-upgrade"
            } else {
                "dnf-plugin-system-upgrade"
            };
            return Err(FtoolError::Upgrade(format!(
                "当前 dnf 不支持 system-upgrade，请先安装:\n  sudo dnf install {pkg}"
            )));
        }
        Ok(())
    }

    /// [2/3] 下载目标版本软件包（可选禁用 COPR 仓库防止依赖冲突）
    fn download_packages(next: u32) -> Result<(), FtoolError> {
        println!("\n[2/3] 下载 Fedora {next} 软件包...");
        let ver = next.to_string();
        let download_args: Vec<&OsStr> = {
            let mut args = Vec::new();
            if Prompter::confirm(
                "是否禁用 COPR 仓库防止冲突？ [y/N]: ",
                false,
                NonTerminal::Deny,
            ) {
                args.push(OsStr::new("--setopt=copr:*.enabled=0"));
            }
            args.extend_from_slice(&[
                OsStr::new("system-upgrade"),
                OsStr::new("download"),
                OsStr::new("--releasever"),
                OsStr::new(&ver),
            ]);
            args
        };
        let status = CommandRunner::run_status("dnf", &download_args)?;
        CommandRunner::ensure_success(status)
    }

    /// [3/3] 询问是否立即重启执行离线升级；否则打印手动触发命令
    fn reboot_or_hint() -> Result<(), FtoolError> {
        println!("\n[3/3] 准备重启升级...");
        if Prompter::confirm("是否立即重启执行升级？ [y/N]: ", false, NonTerminal::Deny)
        {
            println!("正在触发离线升级...");
            // 触发重启的命令随 dnf 世代而异，两者互不兼容：
            // DNF5 使用 `offline-upgrade reboot`，DNF4 使用 `system-upgrade reboot`。
            // 必须以实际执行的 dnf 为准，否则升级包已下载却永远无法触发。
            let reboot_args: &[&OsStr] = if Self::dnf_is_dnf5() {
                &[OsStr::new("offline-upgrade"), OsStr::new("reboot")]
            } else {
                &[OsStr::new("system-upgrade"), OsStr::new("reboot")]
            };
            let status = CommandRunner::run_status("dnf", reboot_args.iter().copied())?;
            CommandRunner::ensure_success(status)?;
        } else {
            let hint = if Self::dnf_is_dnf5() {
                "dnf offline-upgrade reboot"
            } else {
                "dnf system-upgrade reboot"
            };
            println!("\n稍后可手动执行: sudo {hint}");
        }
        Ok(())
    }

    /// 判断系统上 `dnf` 命令实际属于哪个 DNF 世代
    ///
    /// dnf5 的 `--version` 输出首行以 "dnf5" 开头，dnf4 以 "dnf" 开头，
    /// 因此以实际命令输出为准，避免被两个世代并存时的包名/符号链接迷惑。
    fn dnf_is_dnf5() -> bool {
        CommandRunner::run("dnf", ["--version"])
            .map(|o| {
                let stdout = String::from_utf8_lossy(&o.stdout);
                stdout
                    .lines()
                    .next()
                    .map(|line| line.starts_with("dnf5"))
                    .unwrap_or(false)
            })
            .unwrap_or(false)
    }

    /// 探测 dnf 是否支持 system-upgrade 子命令
    ///
    /// DNF4 需 dnf-plugin-system-upgrade；DNF5 自 Fedora 42 起内建，
    /// 更早版本需 dnf5-plugin-system-upgrade。`download --help` 无副作用，
    /// 支持时退出码为 0。
    fn supports_system_upgrade() -> bool {
        CommandRunner::run("dnf", ["system-upgrade", "download", "--help"])
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
}

/// 处理 `-U` 命令：root 检查后执行系统升级
pub fn handle_command() -> Result<(), FtoolError> {
    Privilege::ensure_root().and_then(|_| Upgrader::perform_upgrade())
}

#[cfg(test)]
mod tests {
    use super::Upgrader;

    // ---------- 版本号解析 ----------

    #[test]
    fn fedora_release_typical() {
        let out = Upgrader::release_from_fedora_release("Fedora release 40 (Forty)");
        assert_eq!(out, Some(40));
    }

    #[test]
    fn fedora_release_rawhide() {
        // Rawhide 没有版本号数字：应返回 None，由调用方继续降级解析
        assert_eq!(
            Upgrader::release_from_fedora_release("Fedora release Rawhide"),
            None
        );
    }

    #[test]
    fn fedora_release_missing_keyword() {
        assert_eq!(
            Upgrader::release_from_fedora_release("no version here"),
            None
        );
    }

    #[test]
    fn os_release_quoted_version() {
        let content = "NAME=\"Fedora Linux\"\nVERSION=\"40 (Forty)\"\nVERSION_ID=\"40\"\n";
        assert_eq!(Upgrader::release_from_os_release(content), Some(40));
    }

    #[test]
    fn os_release_unquoted_version() {
        let content = "VERSION_ID=41\nID=fedora\n";
        assert_eq!(Upgrader::release_from_os_release(content), Some(41));
    }

    #[test]
    fn os_release_missing() {
        assert_eq!(Upgrader::release_from_os_release("ID=fedora\n"), None);
    }
}
