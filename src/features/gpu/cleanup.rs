//! 清理 ftool 生成的系统配置文件，并恢复/清理 SDDM Xsetup。
//!
//! 决策（哪些文件可删、Xsetup 如何恢复）由纯函数 `decide_xsetup_cleanup`
//! 承担，本模块只负责 IO 执行；`cleanup()` 仅为三步的编排。

use super::file_io::write_file_atomic_mode;
use crate::core::FtoolError;
use crate::features::gpu::constants::*;
use log::{debug, info, warn};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// 判断文件内容是否带 ftool 生成标记（行级匹配）
///
/// 任一非空行 trim 后与 FTOOL_MARKER 相等即视为 ftool 生成。与
/// content.starts_with(FTOOL_MARKER) 的首字节匹配不同，行级匹配兼容 ftool
/// 自身产物的两类首行布局：以 shebang 开头的脚本（XRANDR_BRIDGE_SCRIPT 以
/// "#!/bin/sh" 开头，标记在第二行）与以换行开头的模板（udev/modeset 内容，
/// 标记同样在第二行）。仅用于"是否为 ftool 产物"的归属判定，不改动各调用
/// 点的清理决策逻辑。
pub(super) fn content_has_ftool_marker(content: &[u8]) -> bool {
    content
        .split(|&b| b == b'\n')
        .any(|line| !line.is_empty() && String::from_utf8_lossy(line).trim() == FTOOL_MARKER)
}

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

/// 切换清理时需删除的 ftool 自有配置文件（模块级常量：remove_owned_configs
/// 引用，同时供测试断言删除清单与 SNAPSHOT_PATHS 快照清单的覆盖关系）
const OWNED_CONFIG_PATHS: &[&str] = &[
    MODPROBE_GPU_PATH,
    MODESET_PATH,
    UDEV_INTEGRATED_PATH,
    UDEV_PM_PATH,
    PRIME_DISCRETE_PATH,
    // NVIDIA 独显模式配置
    NV_ENV_PATH,
    EXTRA_XORG_NVIDIA_PATH,
    XORG_CONF_NVIDIA_PATH,
    // 备用路径：/etc/X11/xorg.conf.d 缺失的发行版上为活动写入路径（display.rs），
    // 与主路径同样需要清理，避免切回 integrated 后 PrimaryGPU 配置残留
    XORG_CONF_NVIDIA_FALLBACK_PATH,
    LIGHTDM_SCRIPT_PATH,
    LIGHTDM_CONFIG_PATH,
    // 注意：/lib/udev/rules.d/ 下的文件由包管理器管理，不在此处删除
];

/// 删除前备份目录：保留每个被删文件的最近一次内容副本
///
/// 文件可能含用户手工追加的非 ftool 行（把 ftool 模板改造后自建、或在
/// ftool 生成文件上补充自定义规则）。归属判定只能识别"是否含标记行"，
/// 无法区分文件内哪些行属于 ftool——整删前留一份副本，用户可随时从备份
/// 找回被清理的自定义内容，删除不再"静默且不可逆"。
const CLEANUP_BACKUP_DIR: &str = "/var/cache/ftool/cleanup-backup";

/// 删除前把待删文件内容备份到 [`CLEANUP_BACKUP_DIR`]（尽力而为，失败仅告警）
///
/// 备份为原子写（同 file_io 原语），同名备份每次覆盖为最近一次内容；
/// 备份失败不阻断清理——残留配置对模式切换的危害大于备份缺失，
/// 但失败必须提示，否则用户无从得知备份缺失。
fn backup_before_remove(path: &str) {
    let Some(file_name) = Path::new(path).file_name() else {
        return;
    };
    let backup_path = format!("{}/{}", CLEANUP_BACKUP_DIR, file_name.to_string_lossy());
    match fs::read(path) {
        Ok(content) => {
            if let Err(e) = write_file_atomic_mode(&backup_path, &content, 0o600) {
                warn!(
                    "备份待删除文件失败（尽力而为）; path={}, backup={}, error={}",
                    path, backup_path, e
                );
            } else {
                debug!("已备份待删除文件; path={}, backup={}", path, backup_path);
            }
        }
        // 文件在判定后被并发删除：与"本就不存在"等价，无需备份
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => warn!("读取待备份文件失败（尽力而为）; path={}, error={}", path, e),
    }
}

/// 同步父目录的目录项到磁盘（尽力而为）
///
/// 与 file_io.rs 写侧（文件 + 目录双 fsync）对称：remove 的 unlink 不经
/// 目录 fsync 可能在断电后未持久化，被删文件"复活"并与新模式配置叠加。
fn sync_parent_dir(path: &Path) {
    if let Some(parent) = path.parent()
        && let Err(e) = fs::File::open(parent).and_then(|dir| dir.sync_all())
    {
        warn!("同步父目录失败（非致命，删除已完成） {:?}: {}", parent, e);
    }
}

/// 确认归属后删除单个配置文件；`Ok(true)` 已删除 / `Ok(false)` 未删除。
///
/// 归属判定（owned 与 legacy 统一使用行级标记匹配）：
/// - 文件名并非全部 ftool 专有（如 50-remove-nvidia.rules、
///   11-nvidia-discrete.conf 可能与 NVIDIA 官方教程或 system76-power 等
///   第三方工具共用），无条件删除会误删用户自建配置；文件存在但无 ftool
///   标记 → 保留并 warn，不算失败、不中止。
/// - 唯一例外是 /etc/prime-discrete：内容为 on/off/on-demand 纯模式标记
///   （与 system76-power 的互操作约定，无法内嵌注释行），该路径文件只可能
///   由切换工具创建且每次切换末尾都会按新目标重写，故仍无条件删除。
///
/// 确认归属后删除前先备份原内容（见 [`backup_before_remove`]），随后删除并
/// 同步父目录。`Err` 仅表示删除动作失败，必须中止流程：例如残留的
/// integrated udev 移除规则会在随后的 PCI rescan/重启中再次移除 NVIDIA
/// 设备，造成"报告成功实际失败"；do_switch 的快照回滚机制会负责还原其余
/// 已被删除的配置。
fn remove_config_if_ftool_owned(path: &str) -> Result<bool, FtoolError> {
    if path == PRIME_DISCRETE_PATH {
        if let Err(e) = fs::remove_file(path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            return Err(FtoolError::Gpu(format!(
                "删除配置文件失败; path={}, error={}",
                path, e
            )));
        }
        return Ok(true);
    }

    match fs::read(path) {
        Ok(content) => {
            if !content_has_ftool_marker(&content) {
                warn!("文件存在但无 ftool 标记，跳过删除; path={}", path);
                return Ok(false);
            }
            backup_before_remove(path);
            if let Err(e) = fs::remove_file(path)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                return Err(FtoolError::Gpu(format!(
                    "删除配置文件失败; path={}, error={}",
                    path, e
                )));
            }
            sync_parent_dir(Path::new(path));
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // 文件不存在，无需处理
            Ok(false)
        }
        Err(e) => {
            // 无法读取即无法确认归属，保守保留
            warn!("读取 {} 失败，跳过删除: {}", path, e);
            Ok(false)
        }
    }
}

/// 删除 ftool 自有配置文件（/etc 下）
///
/// 删除失败必须中止切换流程：remove_config_if_ftool_owned 的 Err 仅表示
/// "确认归属后的删除动作失败"（残留配置会与新模式冲突）；文件不存在或无
/// ftool 标记返回 Ok(false) 不中止。
fn remove_owned_configs() -> Result<(), FtoolError> {
    for path in OWNED_CONFIG_PATHS {
        debug!("尝试删除文件; path={}", path);
        remove_config_if_ftool_owned(path)?;
    }
    Ok(())
}

/// 旧版遗留文件删除清单（模块级常量：remove_legacy_configs 引用，同时供测试
/// 断言删除清单与 SNAPSHOT_PATHS 快照清单的覆盖关系）
const LEGACY_CONFIG_PATHS: &[&str] = &[
    "/etc/X11/xorg.conf",
    "/etc/X11/xorg.conf.d/10-nvidia.conf",
    "/etc/X11/xorg.conf.d/90-nvidia.conf",
    "/etc/lightdm/nvidia.sh",
    "/etc/lightdm/lightdm.conf.d/20-nvidia.conf",
    "/etc/gdm/Init/Default",
    "/etc/gdm/custom.conf",
    LEGACY_BLACKLIST_PATH,
    LEGACY_MODESET_PATH,
];

/// 删除旧版兼容遗留文件（升级前的旧路径，如 nvidia-xconfig 产物）
///
/// 与 owned 清单共用同一归属判定、备份与删除逻辑（行级标记匹配；不存在
/// 或无 ftool 标记则保留）。区别仅在于：这些路径多为早期版本覆盖写入的
/// 第三方/用户文件，删除失败只记录 warn、不中止流程。
fn remove_legacy_configs() {
    for path in LEGACY_CONFIG_PATHS {
        if let Err(e) = remove_config_if_ftool_owned(path) {
            warn!("无法删除文件; path={}, error={}", path, e);
        }
    }
}

/// 恢复/清理 SDDM Xsetup：恢复/清理动作由纯函数 `decide_xsetup_cleanup`
/// 决策，此处仅执行 IO（读取、按动作写入/删除/恢复）。
fn restore_sddm_xsetup() -> Result<(), FtoolError> {
    match fs::read(SDDM_XSETUP_BAK_PATH) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // 无备份（旧版本可能未保留备份）：仅删除能确认由 ftool 生成的 Xsetup
            if let Ok(content) = fs::read(SDDM_XSETUP_PATH)
                && content_has_ftool_marker(&content)
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
            let bak_is_ftool = content_has_ftool_marker(&bak);
            let xsetup_exists = Path::new(SDDM_XSETUP_PATH).exists();
            let xsetup_is_ftool = fs::read(SDDM_XSETUP_PATH)
                .map(|c| content_has_ftool_marker(&c))
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
                    // 已知限制：原子写以临时文件 + rename 重建 inode，仅保留备份的
                    // mode；owner/xattr/SELinux context 不会从备份继承，按新文件的
                    // 默认值处理（/usr/share/sddm 下通常为 root:root 与目录默认
                    // 策略，实际影响有限）
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

/// 清理所有由 ftool 生成的系统配置文件
pub(super) fn cleanup() -> Result<(), FtoolError> {
    info!("🧹 清理旧的配置文件...");
    remove_owned_configs()?;
    remove_legacy_configs();
    restore_sddm_xsetup()
}

#[cfg(test)]
mod tests {
    use super::LEGACY_CONFIG_PATHS;
    use super::OWNED_CONFIG_PATHS;
    use super::SNAPSHOT_PATHS;
    use super::XsetupCleanup;
    use super::content_has_ftool_marker;
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

    // ---------- ftool 生成标记归属判定（行级匹配） ----------

    #[test]
    fn marker_matches_shebang_script_second_line() {
        // XRANDR_BRIDGE_SCRIPT 以 "#!/bin/sh" 开头、标记在第二行：此前用
        // starts_with 首字节匹配时，ftool 自写的 SDDM Xsetup 恒被判为
        // "非 ftool 生成"，cleanup 恢复决策走 Keep，桥接脚本与 .bak 永久残留
        let shebang = b"#!/bin/sh\n# Automatically generated by ftool\nxrandr --auto\n";
        assert!(content_has_ftool_marker(shebang));
    }

    #[test]
    fn marker_matches_first_line_content() {
        // 无 shebang 的模板（xorg/modprobe 等）标记位于首行的既有布局
        assert!(content_has_ftool_marker(
            b"# Automatically generated by ftool\nblacklist nouveau\n"
        ));
    }

    #[test]
    fn marker_matches_after_blank_or_comment_lines() {
        // ftool 的 udev/modeset 模板内容以换行开头（标记在第二行）；用户也可能
        // 在文件头追加自己的注释行，行级匹配均不受影响
        assert!(content_has_ftool_marker(
            b"\n# Automatically generated by ftool\n"
        ));
        assert!(content_has_ftool_marker(
            b"# user comment line\n\n# Automatically generated by ftool\n"
        ));
    }

    #[test]
    fn marker_rejects_user_content_and_empty() {
        // 纯用户脚本/无标记内容/空内容均不属于 ftool 产物
        assert!(!content_has_ftool_marker(b"#!/bin/sh\nxrandr --auto\n"));
        assert!(!content_has_ftool_marker(b"blacklist nouveau\n"));
        assert!(!content_has_ftool_marker(b""));
        assert!(!content_has_ftool_marker(b"\n\n"));
    }

    // ---------- 删除清单与快照清单一致性 ----------

    #[test]
    fn removable_paths_all_covered_by_snapshot() {
        // remove_owned_configs（ftool 自有配置）与 remove_legacy_configs（旧版遗留
        // 文件）删除的路径必须全部被 SNAPSHOT_PATHS 覆盖：切换中途失败按快照回滚
        // 已删除的文件，两个清单一旦漂移，回滚会漏掉本应还原的路径（SDDM Xsetup
        // 及其 .bak 均在快照清单内）。两清单已提升为模块级常量，测试直接引用，
        // 避免在测试里重复字面量随清单漂移
        let mut removable: Vec<&str> = Vec::new();
        removable.extend_from_slice(OWNED_CONFIG_PATHS);
        removable.extend_from_slice(LEGACY_CONFIG_PATHS);
        assert!(!removable.is_empty(), "删除清单不应为空");
        for path in removable {
            assert!(
                SNAPSHOT_PATHS.contains(&path),
                "删除目标 {} 未被 SNAPSHOT_PATHS 覆盖，切换失败时将无法回滚",
                path
            );
        }
    }
}
