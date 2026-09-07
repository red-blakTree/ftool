//! 系统配置文件的原子写入原语。
//!
//! ftool 写入 /etc 等目录的配置文件统一经此落盘：临时文件 + fsync + rename
//! 保证断电/崩溃窗口最小，权限在 rename 前一次到位，无"先放宽再收窄"窗口。

use crate::core::FtoolError;
use log::debug;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// 原子方式写入文件并设置精确权限。
///
/// 实现：先以 0600 独占创建临时文件（拒绝预置符号链接，写入期间权限收紧），
/// 写入并落盘后在 rename 前把临时文件权限设为目标 mode 再原子替换——全程临时
/// 文件权限不宽于目标 mode，不存在先放宽再收窄的瞬时窗口（回滚恢复 0700/0600
/// 等收紧权限的文件时尤其重要）。
pub(super) fn write_file_atomic_mode(
    path: &str,
    content: &[u8],
    mode: u32,
) -> Result<(), FtoolError> {
    if let Some(parent) = Path::new(path).parent() {
        fs::create_dir_all(parent)
            .map_err(|e| FtoolError::Gpu(format!("创建目录失败 {:?}: {}", parent, e)))?;
    }

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let tmp_path = format!("{}.tmp.{}-{}", path, std::process::id(), ts);

    // O_EXCL（create_new）独占创建：拒绝预置同名文件/符号链接；创建期先用
    // 0600 收紧权限，避免 umask=000 时内容写入期间出现全局可写窗口
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(&tmp_path).map_err(|e| {
        // 打开失败时 tmp 可能并非本进程创建（预置文件），不擅自删除
        FtoolError::Gpu(format!("创建临时文件失败 {}: {}", tmp_path, e))
    })?;
    file.write_all(content).map_err(|e| {
        let _ = std::fs::remove_file(&tmp_path); // 清理本次创建的部分写入文件
        FtoolError::Gpu(format!("写入临时文件失败 {}: {}", tmp_path, e))
    })?;
    drop(file);
    debug!("临时文件已写入; path={}", tmp_path);

    // 在 rename 前把权限设为目标 mode（由 0600 收紧态直接放宽到目标值，
    // 不经过比目标权限更宽的中间态）
    fs::set_permissions(&tmp_path, fs::Permissions::from_mode(mode)).map_err(|e| {
        let _ = std::fs::remove_file(&tmp_path);
        FtoolError::Gpu(format!("设置权限失败 {}: {}", tmp_path, e))
    })?;
    debug!("已设置文件权限; path={}, mode={:o}", tmp_path, mode);

    // 写后先同步到磁盘再 rename，缩小断电留下空/损坏文件的窗口
    fs::File::open(&tmp_path)
        .and_then(|f| f.sync_all())
        .map_err(|e| {
            let _ = fs::remove_file(&tmp_path);
            FtoolError::Gpu(format!("同步临时文件到磁盘失败 {}: {}", tmp_path, e))
        })?;
    fs::rename(&tmp_path, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp_path);
        FtoolError::Gpu(format!("重命名文件到 {} 失败: {}", path, e))
    })?;
    // 同步父目录，确保 rename 的目录项落盘（Linux 允许 fsync 目录；尽力而为）
    if let Some(parent) = Path::new(path).parent()
        && let Ok(dir) = fs::File::open(parent)
    {
        let _ = dir.sync_all();
    }
    debug!("配置文件已生效; path={}", path);
    Ok(())
}

/// 原子方式写入文件（可执行文件 0755，普通配置文件 0644）
fn write_file_atomic(path: &str, content: &[u8], executable: bool) -> Result<(), FtoolError> {
    write_file_atomic_mode(path, content, if executable { 0o755 } else { 0o644 })
}

/// 原子方式写入文本文件（可选赋予可执行权限）
pub(super) fn create_file(path: &str, content: &str, executable: bool) -> Result<(), FtoolError> {
    write_file_atomic(path, content.as_bytes(), executable)
}

/// 原子方式写入二进制内容文件
pub(super) fn create_file_bytes(path: &str, content: &[u8]) -> Result<(), FtoolError> {
    write_file_atomic(path, content, false)
}

#[cfg(test)]
mod tests {
    use super::write_file_atomic;
    use super::write_file_atomic_mode;
    use std::os::unix::fs::PermissionsExt;

    /// write_file_atomic_mode 落盘文件应带精确目标权限（无放宽窗口）
    #[test]
    fn atomic_write_mode_applies_exact_permissions() {
        let dir = std::env::temp_dir().join(format!("ftool-atomic-test-{}", std::process::id()));
        let path = dir.join("conf");
        write_file_atomic_mode(path.to_str().unwrap(), b"# test\n", 0o600).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "精确权限 0600 应被应用");

        write_file_atomic_mode(path.to_str().unwrap(), b"# test2\n", 0o640).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640, "覆盖写入时同样按新目标权限设置");
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, "# test2\n");

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    /// write_file_atomic 的 executable 语义映射 0755 / 0644
    #[test]
    fn atomic_write_executable_flag_maps_to_standard_modes() {
        let dir = std::env::temp_dir().join(format!("ftool-atomic-test-{}", std::process::id()));
        let path = dir.join("script");
        write_file_atomic(path.to_str().unwrap(), b"#!/bin/sh\n", true).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);

        let path2 = dir.join("data");
        write_file_atomic(path2.to_str().unwrap(), b"x", false).unwrap();
        let mode = std::fs::metadata(&path2).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644);

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&path2);
        let _ = std::fs::remove_dir(&dir);
    }
}
