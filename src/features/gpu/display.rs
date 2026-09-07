//! NVIDIA 独显模式的显示/渲染配置文件写入。
//!
//! 覆盖四类配置，共同目标是让 NVIDIA 成为主显示输出/默认渲染设备：
//! - X11 PrimaryGPU 与额外 Xorg 选项（Coolbits / ForceCompositionPipeline）；
//! - Display Manager（SDDM / LightDM）的 xrandr 桥接脚本；
//! - systemd environment.d 的 NVIDIA 渲染环境变量；
//! - PRIME 离散模式标志（/etc/prime-discrete，与 system76-power 约定一致）。

use super::file_io::create_file;
use crate::core::FtoolError;
use crate::core::runner::CommandRunner;
use crate::features::gpu::NvidiaOptions;
use crate::features::gpu::constants::*;
use log::{debug, info};
use std::fs;
use std::path::Path;

/// 获取 X11 配置文件路径，自动检测可用目录
///
/// 参考 system76-power 的 `get_xorg_conf_path` 实现。
fn get_xorg_conf_path() -> &'static str {
    if Path::new("/etc/X11/xorg.conf.d").exists() {
        XORG_CONF_NVIDIA_PATH
    } else {
        XORG_CONF_NVIDIA_FALLBACK_PATH
    }
}

/// 写入 NVIDIA 离散模式 X11 PrimaryGPU 配置
///
/// 在 Nvidia 模式下设置 PrimaryGPU "Yes"，使 NVIDIA 成为 X11 主显示输出。
/// 参考 system76-power 的 discrete 模式 Xorg 配置逻辑。
pub(super) fn write_xorg_nvidia_config() -> Result<(), FtoolError> {
    let path = get_xorg_conf_path();
    info!("写入 X11 配置; path={}", path);
    create_file(path, XORG_CONF_NVIDIA_CONTENT, false)
}

/// 写入 NVIDIA 额外 Xorg 配置（ForceCompositionPipeline / Coolbits）
///
/// 仅在至少一个选项启用时写入。参考 gswitch 的 extra Xorg 逻辑。
pub(super) fn write_xorg_nvidia_extra_config(opts: &NvidiaOptions) -> Result<(), FtoolError> {
    if opts.force_comp || opts.coolbits.is_some() {
        info!(
            "写入 NVIDIA 额外 Xorg 配置 (force_comp={}, coolbits={:?})",
            opts.force_comp, opts.coolbits
        );
        let mut content = String::from(EXTRA_XORG_HEADER);
        if opts.force_comp {
            content.push_str(EXTRA_XORG_FORCE_COMP);
        }
        if let Some(cb) = opts.coolbits {
            content.push_str(&EXTRA_XORG_COOLBITS.replacen("{}", &cb.to_string(), 1));
        }
        content.push_str(EXTRA_XORG_FOOTER);
        create_file(EXTRA_XORG_NVIDIA_PATH, &content, false)
    } else {
        Ok(())
    }
}

/// 写入 Display Manager xrandr 桥接脚本（SDDM / LightDM）
///
/// 在没有 MUX 切换器的笔记本上，外接显示器的物理端口通常连接到 NVIDIA GPU，
/// 需要在 DM 启动时通过 xrandr 将 iGPU 的输出桥接到 NVIDIA。参考 gswitch。
pub(super) fn write_dm_scripts() -> Result<(), FtoolError> {
    // 检测 iGPU provider 名称
    let igpu_provider = detect_igpu_xrandr_provider();
    let script = XRANDR_BRIDGE_SCRIPT.replacen("{}", &igpu_provider, 1);

    // SDDM：仅当 Xsetup 尚非 ftool 生成且尚无备份时才备份原始文件（fs::copy
    // 保留原始权限位），避免重复切换时把 ftool 脚本自身当成备份覆盖真正的
    // 原始内容；随后始终以原子写覆盖为当前桥接脚本（幂等）
    if Path::new(SDDM_XSETUP_PATH).exists() {
        info!("检测到 SDDM，写入 xrandr 桥接脚本");
        let existing = fs::read(SDDM_XSETUP_PATH)
            .map_err(|e| FtoolError::Gpu(format!("读取 SDDM Xsetup 失败，无法安全写入: {}", e)))?;
        let is_ftool_generated = existing.starts_with(FTOOL_MARKER.as_bytes());
        if !is_ftool_generated && !Path::new(SDDM_XSETUP_BAK_PATH).exists() {
            fs::copy(SDDM_XSETUP_PATH, SDDM_XSETUP_BAK_PATH)
                .map_err(|e| FtoolError::Gpu(format!("备份 SDDM Xsetup 失败: {}", e)))?;
        }
        create_file(SDDM_XSETUP_PATH, &script, true)?;
    }

    // LightDM
    if Path::new("/etc/lightdm").is_dir() {
        info!("检测到 LightDM，写入 xrandr 桥接脚本");
        create_file(LIGHTDM_SCRIPT_PATH, &script, true)?;
        create_file(LIGHTDM_CONFIG_PATH, LIGHTDM_CONFIG_CONTENT, false)?;
    }

    Ok(())
}

/// xrandr 检测超时时间（秒）
const XRANDR_TIMEOUT_SECS: u64 = 5;

/// 检测 iGPU 的 xrandr provider 名称
///
/// 优先取 xrandr --listproviders 中非 NVIDIA 的 provider，
/// 失败或名称不含法时回退到 "modesetting"（Intel/AMD 现代驱动均兼容）。
///
/// 该名称会拼入以 root 执行的 DM 启动脚本（xrandr --setprovideroutputsource），
/// 因此必须做白名单校验，仅接受常规 provider 名（字母/数字/下划线/连字符/点），
/// 其余一律回退，杜绝异常输出演变为 root 上下文注入的可能。
fn detect_igpu_xrandr_provider() -> String {
    if let Ok(output) =
        CommandRunner::run_with_timeout("xrandr", ["--listproviders"], XRANDR_TIMEOUT_SECS)
    {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            if line.contains("name:")
                && !line.contains("NVIDIA")
                && let Some(name_part) = line.split("name:").nth(1)
            {
                let name = name_part.trim();
                if !name.is_empty()
                    && name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
                {
                    debug!("检测到 iGPU xrandr provider: {}", name);
                    return name.to_string();
                }
                debug!("跳过不含法的 xrandr provider 名称: {:?}", name);
            }
        }
    }
    "modesetting".to_string()
}

/// 写入 NVIDIA 独显模式环境变量配置
///
/// 在 Nvidia 模式下写入 /etc/environment.d/ftool-nvidia.conf，
/// 设置 __NV_PRIME_RENDER_OFFLOAD=1、__GLX_VENDOR_LIBRARY_NAME=nvidia、
/// Vulkan Optimus/ICD 环境变量，确保应用默认使用 NVIDIA 渲染。
pub(super) fn write_nvidia_env_config() -> Result<(), FtoolError> {
    info!("写入 NVIDIA 环境变量配置; path={}", NV_ENV_PATH);
    create_file(NV_ENV_PATH, NV_ENV_CONTENT, false)
}

/// 写入 PRIME 离散模式标志文件
pub(super) fn set_prime_discrete(mode: &str) -> Result<(), FtoolError> {
    info!("设置 {} 为 {}", PRIME_DISCRETE_PATH, mode);
    create_file(PRIME_DISCRETE_PATH, &format!("{}\n", mode), false)
}
