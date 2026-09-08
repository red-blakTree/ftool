use crate::core::FtoolError;
use crate::features::gpu::constants::CACHE_FILE_PATH;
use log::debug;
use serde::de::Visitor as DeVisitor;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::fs;
use std::path::Path;

/// 当前缓存格式版本，变更不兼容格式时递增此值
const CACHE_VERSION: u32 = 2;

/// vendor/device 在 JSON 中以 "0x10de" 形式的十六进制字符串持久化（与 sysfs、
/// udev 规则的 0x 书写风格一致）；反序列化兼容历史 v2 文件的十进制数字形式
mod hex_u16 {
    use super::*;

    pub fn serialize<S: Serializer>(value: &u16, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format!("0x{:04x}", value))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u16, D::Error> {
        struct HexU16Visitor;

        impl<'de> DeVisitor<'de> for HexU16Visitor {
            type Value = u16;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("十六进制字符串（如 \"0x10de\"）或十进制数字")
            }

            // 旧 v2 缓存以十进制 JSON 数字写入，需继续可读
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<u16, E> {
                u16::try_from(v).map_err(|_| E::custom(format!("超出 u16 范围: {}", v)))
            }

            // u16 语义不接受负值，明确报错而非泛化的类型错误
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<u16, E> {
                Err(E::custom(format!("设备 ID 不能为负: {}", v)))
            }

            // 小数/指数形式不是合法设备 ID
            fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<u16, E> {
                Err(E::custom(format!("设备 ID 必须为整数: {}", v)))
            }

            fn visit_str<E: serde::de::Error>(self, raw: &str) -> Result<u16, E> {
                let trimmed = raw.trim();
                let digits = trimmed
                    .strip_prefix("0x")
                    .or_else(|| trimmed.strip_prefix("0X"))
                    .unwrap_or(trimmed);
                u16::from_str_radix(digits, 16)
                    .map_err(|e| E::custom(format!("无效的十六进制设备 ID {:?}: {}", raw, e)))
            }
        }

        deserializer.deserialize_any(HexU16Visitor)
    }
}

/// NVIDIA 设备 ID（vendor + device），用于 GPU PCIe 断电后恢复设备信息
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct NvidiaDeviceId {
    /// vendor ID，JSON 中以 "0x10de" 十六进制字符串持久化
    #[serde(with = "hex_u16")]
    pub vendor: u16,
    /// device ID，JSON 中以 "0x2d19" 十六进制字符串持久化
    #[serde(with = "hex_u16")]
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
    ///
    /// 读端（`read`）与写端（mod.rs 组装 "PCI:…" 字符串后）共用此校验，
    /// 保证解析路径与校验路径对格式/范围的认知一致。
    pub(super) fn validate_pci_bus(bus: &str) -> bool {
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
        super::file_io::create_file(CACHE_FILE_PATH, &json, false)
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
        debug!("删除缓存文件; path={}", CACHE_FILE_PATH);
        match fs::remove_file(CACHE_FILE_PATH) {
            Ok(()) => Ok(()),
            // 文件本就不存在与删除成功等价，无需报错
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(FtoolError::Gpu(format!("删除缓存文件失败: {}", e))),
        }
    }

    /// 查询并返回格式化后的缓存内容
    pub fn query() -> Result<String, FtoolError> {
        match Self::read() {
            Ok(data) => serde_json::to_string_pretty(&data)
                .map_err(|e| FtoolError::Gpu(format!("序列化缓存失败: {}", e))),
            // 缓存从未创建属于正常状态；损坏/版本不符等其他错误应透出以便排障
            Err(_) if !Path::new(CACHE_FILE_PATH).exists() => Ok("无缓存数据".to_string()),
            Err(e) => Err(e),
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

    /// vendor/device 持久化为 "0x…" 十六进制字符串（与 sysfs/udev 书写风格一致）
    #[test]
    fn device_id_serializes_as_hex_string() {
        let id = NvidiaDeviceId {
            vendor: 0x10de,
            device: 0x2d19,
        };
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, r#"{"vendor":"0x10de","device":"0x2d19"}"#);
    }

    /// 读端兼容新格式十六进制字符串（含不带 0x 前缀的写法）
    #[test]
    fn device_id_deserializes_hex_string() {
        let id: NvidiaDeviceId =
            serde_json::from_str(r#"{"vendor":"0x10de","device":"2d19"}"#).unwrap();
        assert_eq!((id.vendor, id.device), (0x10de, 0x2d19));
    }

    /// 读端兼容历史 v2 文件的十进制 JSON 数字，升级后旧缓存仍可用
    #[test]
    fn device_id_deserializes_legacy_decimal_number() {
        let id: NvidiaDeviceId = serde_json::from_str(r#"{"vendor":4318,"device":11545}"#).unwrap();
        assert_eq!((id.vendor, id.device), (0x10de, 0x2d19));
    }

    /// CacheData 整体 round-trip 保持值不变
    #[test]
    fn cache_data_round_trip_preserves_device_ids() {
        let data = CacheData::new(
            "PCI:1:0:0".to_string(),
            vec![
                NvidiaDeviceId {
                    vendor: 0x10de,
                    device: 0x2d19,
                },
                NvidiaDeviceId {
                    vendor: 0x10de,
                    device: 0x22eb,
                },
            ],
        );
        let json = serde_json::to_string(&data).unwrap();
        assert!(json.contains(r#""vendor":"0x10de""#));
        let back: CacheData = serde_json::from_str(&json).unwrap();
        assert_eq!(back.nvidia_device_ids.len(), 2);
        assert_eq!(back.nvidia_device_ids[1].device, 0x22eb);
    }

    /// 负数与小数不是合法设备 ID：明确报错，不静默兜底成 0
    #[test]
    fn device_id_rejects_negative_and_float() {
        let negative: Result<NvidiaDeviceId, _> =
            serde_json::from_str(r#"{"vendor":-1,"device":11545}"#);
        assert!(negative.is_err());
        let float: Result<NvidiaDeviceId, _> =
            serde_json::from_str(r#"{"vendor":4318.5,"device":11545}"#);
        assert!(float.is_err());
    }
}
