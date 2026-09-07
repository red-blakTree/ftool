//! 配置快照：切换模式前保存文件与 systemd 服务状态，失败时恢复原状。

use super::file_io::write_file_atomic_mode;
use super::services::{service_is_enabled, toggle_service};
use crate::core::FtoolError;
use crate::features::gpu::constants::SERVICE_SNAPSHOT_NAMES;
use log::{debug, info, warn};
use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// 单个被快照文件的内容与权限 mode
struct SnapshotFile {
    content: Vec<u8>,
    mode: u32,
}

/// 配置快照：保存一组文件内容和 systemd 服务状态，用于切换失败时恢复原状
pub(super) struct ConfigSnapshot {
    files: HashMap<String, Option<SnapshotFile>>, // path -> file（None = 文件不存在）
    services: HashMap<String, Option<bool>>,      // service -> is-enabled（None = 未知/未安装）
}

impl ConfigSnapshot {
    /// 保存指定路径列表的配置快照和服务状态
    pub(super) fn save(paths: &[&str]) -> Result<Self, FtoolError> {
        let mut files = HashMap::new();
        for &path in paths {
            let entry = if Path::new(path).exists() {
                let metadata = fs::metadata(path).map_err(|e| {
                    FtoolError::Gpu(format!("读取快照文件元数据失败 {}: {}", path, e))
                })?;
                let content = fs::read(path)
                    .map_err(|e| FtoolError::Gpu(format!("读取快照文件失败 {}: {}", path, e)))?;
                let mode = metadata.permissions().mode();
                Some(SnapshotFile { content, mode })
            } else {
                None
            };
            files.insert(path.to_string(), entry);
        }
        debug!("已保存 {} 个文件的配置快照", files.len());

        let mut services = HashMap::new();
        for svc in SERVICE_SNAPSHOT_NAMES {
            services.insert(svc.to_string(), service_is_enabled(svc));
        }
        debug!("已保存 {} 个服务的状态快照", services.len());

        Ok(Self { files, services })
    }

    /// 将配置恢复到快照状态：先恢复文件，再恢复服务
    pub(super) fn restore(&self) {
        info!("正在恢复配置快照...");

        for (path, file) in &self.files {
            match file {
                Some(file) => {
                    // 以快照记录的精确权限一次到位：write_file_atomic_mode 先以 0600
                    // 创建再设为目标权限后 rename，回滚瞬间不会出现先放宽后收窄的
                    // 权限窗口（无需再二次 set_permissions）
                    match write_file_atomic_mode(path, &file.content, file.mode) {
                        Ok(()) => {
                            debug!("已回滚文件: {}", path);
                        }
                        Err(e) => warn!("回滚文件 {} 失败: {}", path, e),
                    }
                }
                None => {
                    if let Err(e) = fs::remove_file(path)
                        && e.kind() != std::io::ErrorKind::NotFound
                    {
                        warn!("回滚时删除 {} 失败: {}", path, e);
                    }
                }
            }
        }

        for (svc, prev_state) in &self.services {
            match prev_state {
                Some(true) => {
                    if let Err(e) = toggle_service(svc, true) {
                        warn!("回滚服务 {} (enable) 失败: {}", svc, e);
                    }
                }
                Some(false) => {
                    if let Err(e) = toggle_service(svc, false) {
                        warn!("回滚服务 {} (disable) 失败: {}", svc, e);
                    }
                }
                None => {
                    debug!("跳过服务 {} 的回滚（快照时状态未知）", svc);
                }
            }
        }

        info!("配置快照恢复完成");
    }
}
