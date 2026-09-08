//! initramfs 重建：模式切换/重置后恢复内核模块与 initramfs 状态。

use crate::core::FtoolError;
use crate::core::runner::CommandRunner;
use log::{debug, info};
use std::path::Path;

/// 重建 initramfs（支持 dracut 和 update-initramfs，以及 OSTree 系统）
pub(super) fn rebuild_initramfs() -> Result<(), FtoolError> {
    info!("⚙️ 正在重建 initramfs...");

    // OSTree 系系统（如 Fedora Silverblue）
    if Path::new("/ostree").exists() || Path::new("/sysroot/ostree").exists() {
        info!("检测到 OSTree 系统，使用 rpm-ostree...");
        let status =
            CommandRunner::run_status("rpm-ostree", ["initramfs", "--enable", "--arg=--force"])?;
        return CommandRunner::ensure_success(status);
    }

    // Debian/Ubuntu 使用 update-initramfs：探测的是绝对路径，执行也必须使用
    // 同一绝对路径——受限 PATH（sudo secure_path/cron 等）下按命令名经 PATH
    // 查找可能失败，报错与真实原因脱节
    for update_initramfs in ["/usr/sbin/update-initramfs", "/sbin/update-initramfs"] {
        if Path::new(update_initramfs).exists() {
            info!("检测到 update-initramfs，使用 Debian 方式...");
            let status = CommandRunner::run_status(update_initramfs, ["-u", "-k", "all"])?;
            return CommandRunner::ensure_success(status);
        }
    }

    // 其他发行版使用 dracut
    let mut cmd: Vec<&str> = vec!["dracut", "--force", "--regenerate-all"];

    // 使用 systemd-inhibit 防止关机中断重建过程
    if Path::new("/usr/bin/systemd-inhibit").exists() {
        debug!("检测到 systemd-inhibit，将使用它防止关机中断重建过程");
        cmd = vec![
            "systemd-inhibit",
            "--who=ftool",
            "--why",
            "Rebuilding initramfs",
            "--",
        ]
        .into_iter()
        .chain(cmd)
        .collect();
    }

    debug!("执行命令; cmd={:?}", cmd);
    let status = CommandRunner::run_status(cmd[0], &cmd[1..])?;
    CommandRunner::ensure_success(status)
}
