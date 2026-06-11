use crate::core::FtoolError;
use crate::core::prompter::Prompter;
use crate::core::runner::CommandRunner;
use std::ffi::OsStr;

/// Fedora 系统版本升级工具
pub struct Upgrader;

impl Upgrader {
    /// 检测当前 Fedora 主版本号
    ///
    /// 优先从 /etc/fedora-release 解析，降级到 /etc/os-release。
    fn fedora_version() -> Result<u32, FtoolError> {
        // 优先解析 /etc/fedora-release
        if let Ok(content) = std::fs::read_to_string("/etc/fedora-release") {
            // 查找 "release" 关键字后的数字
            if let Some(pos) = content.find("release") {
                let rest = &content[pos + 7..];
                for word in rest.split_whitespace() {
                    if let Ok(v) = word.parse() {
                        return Ok(v);
                    }
                }
            }
        }

        // 降级解析 /etc/os-release
        if let Ok(content) = std::fs::read_to_string("/etc/os-release") {
            for line in content.lines() {
                if let Some(v) = line.strip_prefix("VERSION_ID=")
                    && let Ok(n) = v.trim_matches('"').parse()
                {
                    return Ok(n);
                }
            }
        }
        Err(FtoolError::Upgrade("无法检测 Fedora 版本".into()))
    }

    /// 执行 Fedora 系统大版本升级（如 Fedora 40 → 41）
    pub fn perform_upgrade() -> Result<(), FtoolError> {
        let cur = Self::fedora_version()?;
        let next = cur + 1;
        println!("\n⚠️ 即将进行系统升级: Fedora {cur} → {next}");

        if Prompter::is_terminal() && !Prompter::ask_yes("是否继续？ [y/N]: ", false) {
            println!("已取消升级。");
            return Ok(());
        }

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

        // 可选：先更新当前系统
        if Prompter::is_terminal() && Prompter::ask_yes("是否先更新当前系统？ [y/N]: ", false)
        {
            println!("\n📦 更新当前系统...");
            let status = CommandRunner::run_status("dnf", ["upgrade", "--refresh"])?;
            CommandRunner::ensure_success(status)?;
        }

        // 检查 dnf system-upgrade 插件是否可用
        let plugin_check = CommandRunner::run_status("rpm", ["-q", "dnf-plugin-system-upgrade"])?;
        if !plugin_check.success() {
            // DNF5 已将 system-upgrade 内建，无需额外插件
            let dnf5_check = CommandRunner::run_status("rpm", ["-q", "dnf5"])?;
            if !dnf5_check.success() {
                return Err(FtoolError::Upgrade(
                    "未找到 dnf-plugin-system-upgrade，请先安装:\n  sudo dnf install dnf-plugin-system-upgrade".into(),
                ));
            }
        }

        println!("\n[2/3] 下载 Fedora {next} 软件包...");
        let download_args: Vec<&OsStr> = {
            let mut args = Vec::new();
            if Prompter::is_terminal()
                && Prompter::ask_yes("是否禁用 COPR 仓库防止冲突？ [y/N]: ", false)
            {
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
        CommandRunner::ensure_success(status)?;

        println!("\n[3/3] 准备重启升级...");
        if Prompter::is_terminal() && Prompter::ask_yes("是否立即重启执行升级？ [y/N]: ", false)
        {
            println!("正在触发离线升级...");
            let status = CommandRunner::run_status("dnf", ["offline-upgrade", "reboot"])?;
            CommandRunner::ensure_success(status)?;
        } else {
            println!("\n稍后可手动执行: sudo dnf offline-upgrade reboot");
        }
        Ok(())
    }
}
