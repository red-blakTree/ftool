//! 清理 ftool 生成的系统配置文件，并恢复/清理 SDDM Xsetup。
//!
//! 决策（哪些文件可删、Xsetup 如何恢复）由纯函数 `decide_xsetup_cleanup`
//! 承担，本模块只负责 IO 执行。

use super::file_io::write_file_atomic_mode;
use crate::core::FtoolError;
use crate::features::gpu::constants::*;
use log::{debug, info, warn};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// SDDM Xsetup 的清理动作（由决策纯函数返回，IO 由调用方执行）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum XsetupCleanup {
    /// 备份为原始内容且当前 Xsetup 为 ftool 生成（或已被删除）：用备份恢复 Xsetup
    Restore,
    /// 备份本身是 ftool 产物且当前 Xsetup 亦为 ftool 生成：两者均删除
    RemoveXsetupAndBak,
    /// 备份本身是 ftool 产物（原始备份已被早期版本覆盖丢失）：仅删除备份
    RemoveBakOnly,
    /// 当前 Xsetup 非 ftool 生成（用户修改/包更新）：保留现状与备份
    Keep,
}

/// SDDM Xsetup 恢复/清理决策表（纯函数，便于测试）。
///
/// 前提：备份文件已存在（由调用方保证）。备份可能本身是 ftool 生成的脚本——
/// 早期版本在重复切换时会把 ftool 脚本自身写为备份，覆盖掉真正的原始备份，
/// 此时原始内容已不可恢复，只能清理 ftool 痕迹而不是把 ftool 脚本写回 Xsetup。
fn decide_xsetup_cleanup(
    xsetup_exists: bool,
    xsetup_is_ftool: bool,
    bak_is_ftool: bool,
) -> XsetupCleanup {
    if bak_is_ftool {
        if xsetup_is_ftool {
            XsetupCleanup::RemoveXsetupAndBak
        } else {
            XsetupCleanup::RemoveBakOnly
        }
    } else if !xsetup_exists || xsetup_is_ftool {
        // 当前 Xsetup 为 ftool 生成（或已被删除）：用原始备份恢复
        XsetupCleanup::Restore
    } else {
        // 当前 Xsetup 非 ftool 生成：可能是包更新或用户手工修改，不覆盖
        XsetupCleanup::Keep
    }
}

/// 清理所有由 ftool 生成的系统配置文件
pub(super) fn cleanup() -> Result<(), FtoolError> {
    info!("🧹 清理旧的配置文件...");
    // ftool 自身生成的配置路径（仅 /etc/ 下的文件，文件名即所有权，直接删除）
    let to_remove: &[&str] = &[
        MODPROBE_GPU_PATH,
        MODESET_PATH,
        UDEV_INTEGRATED_PATH,
        UDEV_PM_PATH,
        PRIME_DISCRETE_PATH,
        // NVIDIA 独显模式配置
        NV_ENV_PATH,
        EXTRA_XORG_NVIDIA_PATH,
        XORG_CONF_NVIDIA_PATH,
        LIGHTDM_SCRIPT_PATH,
        LIGHTDM_CONFIG_PATH,
        // 注意：/lib/udev/rules.d/ 下的文件由包管理器管理，不在此处删除
    ];

    for path in to_remove {
        debug!("尝试删除文件; path={}", path);
        // ftool 自有文件删除失败必须中止切换流程：例如残留的 integrated udev 移除
        // 规则会在随后的 PCI rescan/重启中再次移除 NVIDIA 设备，造成"报告成功实际
        // 失败"；do_switch 的快照回滚机制会负责还原其余已被删除的配置。
        if let Err(e) = fs::remove_file(path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            return Err(FtoolError::Gpu(format!(
                "删除配置文件失败; path={}, error={}",
                path, e
            )));
        }
    }

    // 旧版兼容路径（用于清理升级前的遗留文件）：
    // 这些文件名并非 ftool 专有，可能由用户手写或 nvidia-xconfig 等第三方工具生成，
    // 因此仅当内容确认带 ftool 生成标记时才删除，防止误删用户自己的配置
    // （切换成功的路径不会触发快照回滚，删除是不可逆的）。
    let legacy_paths: &[&str] = &[
        "/etc/X11/xorg.conf",
        "/usr/share/X11/xorg.conf.d/11-nvidia-discrete.conf",
        "/etc/X11/xorg.conf.d/10-nvidia.conf",
        "/etc/X11/xorg.conf.d/90-nvidia.conf",
        "/etc/lightdm/nvidia.sh",
        "/etc/lightdm/lightdm.conf.d/20-nvidia.conf",
        "/etc/gdm/Init/Default",
        "/etc/gdm/custom.conf",
        LEGACY_BLACKLIST_PATH,
        LEGACY_MODESET_PATH,
    ];

    for path in legacy_paths {
        match fs::read_to_string(path) {
            Ok(content) if content.starts_with(FTOOL_MARKER) => {
                debug!("删除旧版 ftool 生成的遗留文件; path={}", path);
                if let Err(e) = fs::remove_file(path)
                    && e.kind() != std::io::ErrorKind::NotFound
                {
                    warn!("无法删除文件; path={}, error={}", path, e);
                }
            }
            Ok(_) => {
                warn!(
                    "跳过删除 {}: 文件存在但内容不含 ftool 生成标记，可能为用户自建配置",
                    path
                );
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // 文件不存在，无需处理
            }
            Err(e) => {
                warn!("读取 {} 失败，跳过删除: {}", path, e);
            }
        }
    }

    // 还原 SDDM Xsetup：恢复/清理动作由纯函数 decide_xsetup_cleanup 决策，
    // 此处仅执行 IO（读取、按动作写入/删除/恢复）
    match fs::read(SDDM_XSETUP_BAK_PATH) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // 无备份（旧版本可能未保留备份）：仅删除能确认由 ftool 生成的 Xsetup
            if let Ok(content) = fs::read(SDDM_XSETUP_PATH)
                && content.starts_with(FTOOL_MARKER.as_bytes())
            {
                debug!(
                    "删除旧版 ftool 生成的 SDDM Xsetup; path={}",
                    SDDM_XSETUP_PATH
                );
                fs::remove_file(SDDM_XSETUP_PATH)
                    .map_err(|e| FtoolError::Gpu(format!("删除 SDDM Xsetup 失败: {}", e)))?;
            }
        }
        Err(e) => {
            warn!("读取 SDDM Xsetup 备份失败: {}", e);
        }
        Ok(bak) => {
            let bak_is_ftool = bak.starts_with(FTOOL_MARKER.as_bytes());
            let xsetup_exists = Path::new(SDDM_XSETUP_PATH).exists();
            let xsetup_is_ftool = fs::read(SDDM_XSETUP_PATH)
                .map(|c| c.starts_with(FTOOL_MARKER.as_bytes()))
                .unwrap_or(false);
            match decide_xsetup_cleanup(xsetup_exists, xsetup_is_ftool, bak_is_ftool) {
                XsetupCleanup::Restore => {
                    debug!("还原 SDDM Xsetup 备份; path={}", SDDM_XSETUP_PATH);
                    let mode = fs::metadata(SDDM_XSETUP_BAK_PATH)
                        .map_err(|e| {
                            FtoolError::Gpu(format!("读取 SDDM Xsetup 备份元数据失败: {}", e))
                        })?
                        .permissions()
                        .mode();
                    // 按备份的原始 mode 一次到位（写入过程不经过比目标更宽的权限）
                    write_file_atomic_mode(SDDM_XSETUP_PATH, &bak, mode)?;
                    fs::remove_file(SDDM_XSETUP_BAK_PATH).map_err(|e| {
                        FtoolError::Gpu(format!("删除 SDDM Xsetup 备份失败: {}", e))
                    })?;
                }
                XsetupCleanup::RemoveXsetupAndBak => {
                    warn!(
                        "SDDM Xsetup 备份内容为 ftool 自身生成（原始备份可能已被早期版本覆盖丢失），删除 ftool 痕迹; backup={}",
                        SDDM_XSETUP_BAK_PATH
                    );
                    if let Err(e) = fs::remove_file(SDDM_XSETUP_PATH) {
                        warn!("删除 ftool 生成的 SDDM Xsetup 失败: {}", e);
                    }
                    fs::remove_file(SDDM_XSETUP_BAK_PATH).map_err(|e| {
                        FtoolError::Gpu(format!("删除 SDDM Xsetup 备份失败: {}", e))
                    })?;
                }
                XsetupCleanup::RemoveBakOnly => {
                    warn!(
                        "SDDM Xsetup 备份内容为 ftool 自身生成（原始备份可能已被早期版本覆盖丢失），保留当前 Xsetup 并删除备份; backup={}",
                        SDDM_XSETUP_BAK_PATH
                    );
                    fs::remove_file(SDDM_XSETUP_BAK_PATH).map_err(|e| {
                        FtoolError::Gpu(format!("删除 SDDM Xsetup 备份失败: {}", e))
                    })?;
                }
                XsetupCleanup::Keep => {
                    warn!(
                        "当前 {} 非 ftool 生成（可能已被包更新或用户修改），跳过备份恢复并保留备份",
                        SDDM_XSETUP_PATH
                    );
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::XsetupCleanup;
    use super::decide_xsetup_cleanup;

    // ---------- SDDM Xsetup 清理决策表 ----------

    #[test]
    fn xsetup_ftool_bak_with_ftool_current_removes_both() {
        assert_eq!(
            decide_xsetup_cleanup(true, true, true),
            XsetupCleanup::RemoveXsetupAndBak
        );
    }

    #[test]
    fn xsetup_ftool_bak_keeps_non_ftool_current() {
        // 备份为 ftool 产物（原始备份已丢失）但当前 Xsetup 非 ftool：仅删备份，保留用户内容
        assert_eq!(
            decide_xsetup_cleanup(true, false, true),
            XsetupCleanup::RemoveBakOnly
        );
        assert_eq!(
            decide_xsetup_cleanup(false, false, true),
            XsetupCleanup::RemoveBakOnly
        );
    }

    #[test]
    fn xsetup_original_bak_restores_ftool_or_missing_current() {
        // 备份为原始内容：当前 Xsetup 为 ftool 生成（或被删除）时用备份恢复
        assert_eq!(
            decide_xsetup_cleanup(true, true, false),
            XsetupCleanup::Restore
        );
        assert_eq!(
            decide_xsetup_cleanup(false, false, false),
            XsetupCleanup::Restore
        );
    }

    #[test]
    fn xsetup_original_bak_keeps_user_modified_current() {
        // 备份为原始内容但当前 Xsetup 已被用户/包更新修改：不覆盖
        assert_eq!(
            decide_xsetup_cleanup(true, false, false),
            XsetupCleanup::Keep
        );
    }
}
