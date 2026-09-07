//! systemd 服务启停与状态查询（NVIDIA persistenced/fallback/suspend 相关）。

use crate::core::FtoolError;
use crate::core::runner::CommandRunner;
use crate::features::gpu::constants::NVIDIA_SUSPEND_SERVICES;
use log::{info, warn};

/// 启用或禁用 systemd 服务
///
/// 当服务操作失败时返回 `FtoolError::Process`。
/// 调用方可根据场景决定是否忽略（如 `configure_nvidia_suspend_services`）。
pub(super) fn toggle_service(name: &str, enable: bool) -> Result<(), FtoolError> {
    let action = if enable { "enable" } else { "disable" };
    let status = CommandRunner::run_status("systemctl", [action, name])?;
    if status.success() {
        info!("已成功变更服务状态; action={}, service={}", action, name);
        Ok(())
    } else {
        Err(FtoolError::Process(format!(
            "{} {} 失败 (exit: {:?})",
            action,
            name,
            status.code()
        )))
    }
}

/// 查询 systemd 服务是否已启用
///
/// 返回 `Some(true)` 表示已启用，`Some(false)` 表示已禁用，
/// `None` 表示服务未安装或查询失败（无法用于回滚）。
pub(super) fn service_is_enabled(name: &str) -> Option<bool> {
    let output = CommandRunner::run("systemctl", ["is-enabled", name]).ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let state = stdout.trim();
    match state {
        "enabled" => Some(true),
        "disabled" => Some(false),
        // static（无 [Install] 段，enable/disable 均无意义）、linked/masked/
        // indirect/not-found 等状态无法简单用 enable/disable 还原，视为"未知"，
        // 回滚时跳过，避免对 static 服务误执行 enable 产生误导性错误
        _ => None,
    }
}

/// 管理 NVIDIA 挂起/休眠/恢复服务的启用/禁用
///
/// 为 NVIDIA_SUSPEND_SERVICES 列表中每个服务执行 enable/disable。
/// 服务不存在或操作失败时仅记录 warn 级别日志，不会阻断流程。
pub(super) fn configure_nvidia_suspend_services(enable: bool) -> Result<(), FtoolError> {
    for service in NVIDIA_SUSPEND_SERVICES {
        if let Err(e) = toggle_service(service, enable) {
            warn!(
                "NVIDIA 挂起服务操作失败，将忽略此错误; service={}, error={}",
                service, e
            );
        }
    }
    Ok(())
}
