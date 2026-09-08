//! systemd 服务启停与状态查询（NVIDIA persistenced/fallback/suspend 相关）。

use crate::core::FtoolError;
use crate::core::runner::CommandRunner;
use crate::features::gpu::constants::NVIDIA_SUSPEND_SERVICES;
use log::{debug, info};

/// 启用或禁用 systemd 服务
///
/// 当服务操作失败时返回 `FtoolError::Process`。
/// 调用方可根据场景决定是否忽略（预检语义更精确的入口见 `ensure_service_state`）。
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

/// 服务目标状态配置结果
pub(super) enum ServiceConfigOutcome {
    /// 服务已处于/已成功变更为目标状态
    Configured,
    /// 服务预检为 `None`（未安装/static/masked/查询失败），未执行 enable/disable
    Skipped,
}

/// 生成"该服务未能配置"的用户可见说明
///
/// - `reason` 为 `Some`：服务存在但 enable/disable 真实失败（透传错误文本）；
/// - `reason` 为 `None`：预检为 `None` 而跳过（未安装/static/masked）。
pub(super) fn service_config_issue_message(
    service: &str,
    enable: bool,
    reason: Option<&str>,
) -> String {
    match reason {
        Some(reason) => format!("{}: {}", service, reason),
        None => {
            let action = if enable { "启用" } else { "禁用" };
            format!(
                "{}: 未{}（服务未安装或状态不可变更如 static/masked，已跳过）",
                service, action
            )
        }
    }
}

/// 预检后把服务调整到目标状态（配置服务的推荐入口）
///
/// 相比直接 `toggle_service` 更精确：
/// - 服务已处于目标状态 → 直接返回 `Configured`，避免无谓的 systemctl 调用；
/// - 预检为 `None`（未安装/static/masked/查询失败，如 Fedora 未安装
///   nvidia-fallback.service）→ 返回 `Skipped`，**不算错误**，
///   由调用方决定如何提示用户，避免把发行版差异当成硬错误；
/// - 仅当服务存在但 enable/disable 命令执行失败才返回 `Err`。
pub(super) fn ensure_service_state(
    name: &str,
    enable: bool,
) -> Result<ServiceConfigOutcome, FtoolError> {
    match service_is_enabled(name) {
        Some(current) if current == enable => {
            debug!(
                "服务已处于目标状态，跳过变更; service={}, enable={}",
                name, enable
            );
            Ok(ServiceConfigOutcome::Configured)
        }
        Some(_) => {
            toggle_service(name, enable)?;
            Ok(ServiceConfigOutcome::Configured)
        }
        None => {
            debug!(
                "服务状态不可变更（未安装/static/masked），跳过; service={}",
                name
            );
            Ok(ServiceConfigOutcome::Skipped)
        }
    }
}

/// 管理 NVIDIA 挂起/休眠/恢复服务的启用/禁用
///
/// 为 NVIDIA_SUSPEND_SERVICES 列表中每个服务执行 enable/disable（内部先经
/// `ensure_service_state` 预检）。单项失败不阻断流程：返回所有"未能配置"的
/// 服务说明（空 = 全部成功），由调用方（mod.rs 聚合处）统一呈现。
pub(super) fn configure_nvidia_suspend_services(enable: bool) -> Vec<String> {
    let mut problems: Vec<String> = Vec::new();
    for service in NVIDIA_SUSPEND_SERVICES {
        match ensure_service_state(service, enable) {
            Ok(ServiceConfigOutcome::Configured) => {}
            Ok(ServiceConfigOutcome::Skipped) => {
                problems.push(service_config_issue_message(service, enable, None));
            }
            Err(e) => problems.push(service_config_issue_message(
                service,
                enable,
                Some(&e.to_string()),
            )),
        }
    }
    problems
}
