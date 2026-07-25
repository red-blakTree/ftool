use crate::core::FtoolError;
use crate::features::gpu::constants::CACHE_FILE_PATH;
use log::{debug, warn};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

/// 当前缓存格式版本，变更不兼容格式时递增此值
const CACHE_VERSION: u32 = 2;

/// NVIDIA 设备 ID（vendor + device），用于 GPU PCIe 断电后恢复设备信息
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct NvidiaDeviceId {
    pub vendor: u16,
    pub device: u16,
}

/// 缓存数据结构体
#[derive(Serialize, Deserialize, Debug)]
pub struct CacheData {
    /// 缓存格式版本，用于向后兼容校验
    version: u32,
    /// NVIDIA GPU 的 PCI 总线地址
    pub nvidia_gpu_pci_bus: String,
    /// 所有 NVIDIA PCI 设备的 (vendor, device) 对，用于 GPU PCIe 断电后恢复设备信息
    #[serde(default)]
    pub nvidia_device_ids: Vec<NvidiaDeviceId>,
}

impl CacheData {
    /// 创建新的缓存数据
    pub fn new(nvidia_gpu_pci_bus: String, device_ids: Vec<NvidiaDeviceId>) -> Self {
        Self {
            version: CACHE_VERSION,
            nvidia_gpu_pci_bus,
            nvidia_device_ids: device_ids,
        }
    }
}

/// GPU 缓存管理器——负责缓存数据的持久化
///
/// 将检测到的 NVIDIA GPU PCI 地址缓存到 JSON 文件中，
/// 避免在后续操作中重复检测。
pub struct GpuCache;

impl GpuCache {
    /// 校验 PCI 总线地址格式是否为 "PCI:BB:DD:F" 且各段在有效范围内
    ///
    /// - BB (bus): 0–255
    /// - DD (device): 0–31
    /// - F  (function): 0–7
    fn validate_pci_bus(bus: &str) -> bool {
        let parts: Vec<&str> = bus.split(':').collect();
        if parts.len() != 4 || parts[0] != "PCI" {
            return false;
        }
        // 解析并校验范围
        let (Ok(bus), Ok(dev), Ok(func)) = (
            parts[1].parse::<u32>(),
            parts[2].parse::<u32>(),
            parts[3].parse::<u32>(),
        ) else {
            return false;
        };
        bus <= 255 && dev <= 31 && func <= 7
    }

    /// 将缓存数据写入 JSON 文件
    pub fn write(data: &CacheData) -> Result<(), FtoolError> {
        let json = serde_json::to_string_pretty(data)
            .map_err(|e| FtoolError::Gpu(format!("序列化缓存失败: {}", e)))?;
        debug!("写入缓存; path={}", CACHE_FILE_PATH);
        super::helper::create_file(CACHE_FILE_PATH, &json, false)
    }

    /// 从 JSON 文件读取缓存数据，并校验版本号和字段合理性
    pub fn read() -> Result<CacheData, FtoolError> {
        debug!("读取缓存; path={}", CACHE_FILE_PATH);
        let content = fs::read_to_string(CACHE_FILE_PATH)
            .map_err(|e| FtoolError::Gpu(format!("读取缓存文件失败: {}", e)))?;
        let data: CacheData = serde_json::from_str(&content)
            .map_err(|e| FtoolError::Gpu(format!("解析缓存失败: {}", e)))?;

        // 版本校验
        if data.version != CACHE_VERSION {
            return Err(FtoolError::Gpu(format!(
                "缓存版本不匹配 (期望: {}, 实际: {})",
                CACHE_VERSION, data.version
            )));
        }

        // 字段合理性校验
        if !Self::validate_pci_bus(&data.nvidia_gpu_pci_bus) {
            return Err(FtoolError::Gpu(format!(
                "缓存中 PCI 总线地址格式无效: {}",
                data.nvidia_gpu_pci_bus
            )));
        }

        Ok(data)
    }

    /// 删除缓存文件
    pub fn delete() -> Result<(), FtoolError> {
        if Path::new(CACHE_FILE_PATH).exists() {
            debug!("删除缓存文件; path={}", CACHE_FILE_PATH);
            fs::remove_file(CACHE_FILE_PATH)
                .map_err(|e| FtoolError::Gpu(format!("删除缓存文件失败: {}", e)))?;
        }
        Ok(())
    }

    /// 查询并返回格式化后的缓存内容
    pub fn query() -> Result<String, FtoolError> {
        match Self::read() {
            Ok(data) => serde_json::to_string_pretty(&data)
                .map_err(|e| FtoolError::Gpu(format!("序列化缓存失败: {}", e))),
            Err(_) => {
                warn!("无缓存数据或缓存读取失败");
                Ok("无缓存数据".to_string())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_pci_bus_valid() {
        assert!(GpuCache::validate_pci_bus("PCI:1:0:0"));
        assert!(GpuCache::validate_pci_bus("PCI:10:2:1"));
        assert!(GpuCache::validate_pci_bus("PCI:255:31:7"));
    }

    #[test]
    fn test_validate_pci_bus_invalid_prefix() {
        assert!(!GpuCache::validate_pci_bus("pci:1:0:0"));
        assert!(!GpuCache::validate_pci_bus("AGP:1:0:0"));
        assert!(!GpuCache::validate_pci_bus(""));
    }

    #[test]
    fn test_validate_pci_bus_invalid_format() {
        assert!(!GpuCache::validate_pci_bus("PCI:1:0"));
        assert!(!GpuCache::validate_pci_bus("PCI:1:0:0:0"));
        assert!(!GpuCache::validate_pci_bus("PCI:01:00.0"));
        assert!(!GpuCache::validate_pci_bus("PCI:abc:0:0"));
    }

    #[test]
    fn test_validate_pci_bus_empty_parts() {
        assert!(!GpuCache::validate_pci_bus("PCI::0:0"));
        assert!(!GpuCache::validate_pci_bus("PCI:1::0"));
        assert!(!GpuCache::validate_pci_bus("PCI:1:0:"));
    }
}
