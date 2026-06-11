use crate::core::FtoolError;
use crate::features::gpu::constants::*;
use log::{debug, info, warn};
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

/// GPU 检测结果：NVIDIA GPU 列表、AMD GPU 列表、Intel GPU 列表
type GpuInfoResult = (Vec<GpuInfo>, Vec<GpuInfo>, Vec<GpuInfo>);

/// 系统挂起模式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SleepMode {
    /// 现代 S0ix (s2idle) 挂起
    S0ix,
    /// 传统 S3 (deep) 挂起
    S3,
}

/// 集成显卡厂商类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IgpuVendor {
    Intel,
    Amd,
}

impl IgpuVendor {
    /// 返回厂商的字符串表示
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Intel => "intel",
            Self::Amd => "amd",
        }
    }
}

/// GPU 设备信息（通过 sysfs 检测）
#[derive(Debug)]
struct GpuInfo {
    /// PCI 设备 ID，如 "0000:01:00.0"
    pci_id: String,
    /// PCI 设备号，用于匹配 supported-gpus.json 中的 devid
    device_id: u16,
}

/// NVIDIA GPU 设备条目（对应 supported-gpus.json 中的 chips 条目）
///
/// 参考 system76-power 的实现，用于解析 NVIDIA 驱动附带的
/// `/usr/share/doc/nvidia-driver-*/supported-gpus.json` 文件。
#[derive(Debug, Deserialize)]
struct NvidiaDevice {
    /// 设备 ID（十六进制字符串，如 "0x1E90"）
    devid: String,
    /// 子设备 ID
    #[allow(dead_code)]
    subdeviceid: Option<String>,
    /// 子厂商 ID
    #[allow(dead_code)]
    subvendorid: Option<String>,
    /// 设备名称
    #[allow(dead_code)]
    name: String,
    /// 遗留分支（老版本驱动标记）
    #[allow(dead_code)]
    legacybranch: Option<String>,
    /// 设备特性列表（如 "runtimepm"）
    features: Vec<String>,
}

/// supported-gpus.json 的根结构
///
/// NVIDIA 驱动安装在 `/usr/share/doc/nvidia-driver-<version>/supported-gpus.json`
/// 中描述了该驱动支持的所有 GPU 及其特性。
#[derive(Debug, Deserialize)]
struct SupportedGpus {
    chips: Vec<NvidiaDevice>,
}

/// 需要外接显示器由 NVIDIA GPU 驱动的产品型号（参考 system76-power）
const EXTERNAL_DISPLAY_REQUIRES_NVIDIA: &[&str] = &[
    "addw1",
    "addw2",
    "addw3",
    "addw4",
    "addw5",
    "bonw15",
    "bonw15-b",
    "bonw16",
    "gaze14",
    "gaze15",
    "gaze16-3050",
    "gaze16-3060",
    "gaze16-3060-b",
    "gaze17-3050",
    "gaze17-3060-b",
    "gaze20",
    "kudu6",
    "oryp4",
    "oryp4-b",
    "oryp5",
    "oryp6",
    "oryp7",
    "oryp8",
    "oryp9",
    "oryp10",
    "oryp11",
    "oryp12",
    "oryp13",
    "serw13",
    "serw14",
];

/// 默认使用 Discrete (Nvidia) 模式的产品型号
const DEFAULT_DISCRETE_MODELS: &[&str] = &["bonw16"];

/// GPU 检测器——通过 sysfs 检测系统 GPU 硬件信息
pub struct GpuDetector;

impl GpuDetector {
    /// 通过 sysfs 检测所有 GPU 设备，返回 (nvidia_gpus, amd_gpus, intel_gpus)
    ///
    /// 读取失败时仅记录 debug 日志并跳过该设备，避免因单个 sysfs 属性缺失而中断检测。
    fn detect_all_gpus() -> Result<GpuInfoResult, FtoolError> {
        let pci_path = Path::new("/sys/bus/pci/devices");
        if !pci_path.is_dir() {
            return Err(FtoolError::Gpu(
                "/sys/bus/pci/devices 不存在，无法检测 GPU".into(),
            ));
        }

        let mut nvidia_gpus = Vec::new();
        let mut amd_gpus = Vec::new();
        let mut intel_gpus = Vec::new();

        let entries = fs::read_dir(pci_path)
            .map_err(|e| FtoolError::Gpu(format!("读取 PCI 设备目录失败: {}", e)))?;

        // 先收集所有设备名，用于查找同 slot 的不同 function
        let all_devices: Vec<String> = entries
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
            .collect();

        for dev_name in &all_devices {
            let dev_path = pci_path.join(dev_name);

            // 读取 class
            let class_str = match fs::read_to_string(dev_path.join("class")) {
                Ok(s) => s.trim().to_string(),
                Err(e) => {
                    debug!(
                        "读取 sysfs 属性失败; device={}, attr=class, error={}",
                        dev_name, e
                    );
                    String::new()
                }
            };
            let class = u32::from_str_radix(class_str.trim_start_matches("0x"), 16).unwrap_or(0);

            // 只关注显示控制器（class 0x03xxxx）
            if class >> 16 != 0x03 {
                continue;
            }

            // 读取 vendor
            let vendor_str = match fs::read_to_string(dev_path.join("vendor")) {
                Ok(s) => s.trim().to_string(),
                Err(e) => {
                    debug!(
                        "读取 sysfs 属性失败; device={}, attr=vendor, error={}",
                        dev_name, e
                    );
                    String::new()
                }
            };
            let vendor_id =
                u16::from_str_radix(vendor_str.trim_start_matches("0x"), 16).unwrap_or(0);

            // 读取 device
            let device_str = match fs::read_to_string(dev_path.join("device")) {
                Ok(s) => s.trim().to_string(),
                Err(e) => {
                    debug!(
                        "读取 sysfs 属性失败; device={}, attr=device, error={}",
                        dev_name, e
                    );
                    String::new()
                }
            };
            let device_id =
                u16::from_str_radix(device_str.trim_start_matches("0x"), 16).unwrap_or(0);

            let gpu = GpuInfo {
                pci_id: dev_name.clone(),
                device_id,
            };

            match vendor_id {
                0x10DE => {
                    debug!(
                        "发现 NVIDIA GPU; pci_id={}, device_id={:#06x}",
                        dev_name, device_id
                    );
                    nvidia_gpus.push(gpu);
                }
                0x1002 => {
                    debug!("发现 AMD GPU; pci_id={}", dev_name);
                    amd_gpus.push(gpu);
                }
                0x8086 => {
                    debug!("发现 Intel GPU; pci_id={}", dev_name);
                    intel_gpus.push(gpu);
                }
                _ => {
                    debug!(
                        "发现未知 GPU; pci_id={}, vendor={:#06x}",
                        dev_name, vendor_id
                    );
                }
            }
        }

        Ok((nvidia_gpus, amd_gpus, intel_gpus))
    }

    /// 检测系统是否支持 GPU 切换（笔记本 + 同时拥有独显和集显）
    ///
    /// 返回 `Ok(true)` 表示支持切换，`Ok(false)` 表示不支持，
    /// `Err` 表示检测过程失败（如无法读取 PCI 设备目录）。
    pub fn can_switch() -> Result<bool, FtoolError> {
        // 检测是否为台式机（chassis_type == 3 表示 Desktop）
        let chassis = match fs::read_to_string("/sys/class/dmi/id/chassis_type") {
            Ok(s) => s,
            Err(e) => {
                warn!("读取 chassis_type 失败，不以此为排除依据; error={}", e);
                String::new()
            }
        };
        if chassis.trim() == "3" {
            debug!("检测到台式机，不支持 GPU 切换");
            return Ok(false);
        }

        // 检测是否同时拥有 NVIDIA 和集显
        let (nvidia, amd, intel) = Self::detect_all_gpus()?;
        let has_nvidia = !nvidia.is_empty();
        let has_igpu = !amd.is_empty() || !intel.is_empty();
        if has_nvidia && has_igpu {
            info!("系统支持 GPU 切换");
            return Ok(true);
        }

        // sysfs 中未检测到 NVIDIA 显卡（可能在 Integrated 模式下被 udev 移除）
        // 通过缓存或历史配置文件作为后备判断依据
        if has_igpu {
            // 检查 GPU 缓存是否存在（说明之前检测到过 NVIDIA 显卡）
            if Path::new(CACHE_FILE_PATH).exists() {
                info!(
                    "sysfs 未检测到 NVIDIA 但缓存存在，系统仍支持 GPU 切换"
                );
                return Ok(true);
            }
            // 检查 ftool 之前的 modprobe 配置是否存在（说明之前配置过）
            if Path::new(MODPROBE_GPU_PATH).exists() {
                info!(
                    "sysfs 未检测到 NVIDIA 但历史配置存在，系统仍支持 GPU 切换"
                );
                return Ok(true);
            }
        }

        debug!(
            "系统不支持 GPU 切换; has_nvidia={}, has_igpu={}",
            has_nvidia, has_igpu
        );
        Ok(false)
    }

    /// 查询当前 GPU 模式（通过 /proc/modules 和 PRIME 状态综合判断）
    pub fn query_current_mode() -> super::GpuMode {
        // 方法1：检查已加载的内核模块
        let modules = fs::read_to_string("/proc/modules").unwrap_or_default();
        let nvidia_loaded = modules.lines().any(|line| {
            let name = line.split_whitespace().next().unwrap_or("");
            name == "nvidia"
                || name == "nvidia_drm"
                || name == "nvidia_current"
                || name == "nvidia_current_drm"
        });
        let nouveau_loaded = modules.lines().any(|line| {
            let name = line.split_whitespace().next().unwrap_or("");
            name == "nouveau"
        });

        if !nvidia_loaded && !nouveau_loaded {
            return super::GpuMode::Integrated;
        }

        // 方法2：读取 PRIME 离散模式标志
        let prime_mode = fs::read_to_string(PRIME_DISCRETE_PATH)
            .unwrap_or_default()
            .trim()
            .to_string();

        // 方法3：通过配置文件存在性判断
        let has_integrated_config =
            Path::new(MODPROBE_GPU_PATH).exists() && Path::new(UDEV_INTEGRATED_PATH).exists();
        let has_modeset_config = Path::new(MODESET_PATH).exists();

        // 配置与运行时状态不一致的检测
        if has_integrated_config && nvidia_loaded {
            warn!(
                "配置/运行时不匹配: 存在 Integrated 模式的 modprobe 黑名单配置 \
                但 nvidia 内核模块已加载（可能由其他软件或手动操作加载）"
            );
        }

        // 综合判断
        if has_integrated_config && !nvidia_loaded {
            super::GpuMode::Integrated
        } else if prime_mode == "on" {
            // prime-discrete 标志为 "on"，由 ftool switch_mode 写入 → 独显模式
            super::GpuMode::Nvidia
        } else if prime_mode == "on-demand" || (has_modeset_config && nvidia_loaded) {
            // 进一步区分 Compute 和 Hybrid
            // Compute 模式下 nvidia-drm 被黑名单，不存在
            let nvidia_drm_loaded = modules.lines().any(|line| {
                let name = line.split_whitespace().next().unwrap_or("");
                name == "nvidia_drm" || name == "nvidia_current_drm"
            });
            // 额外检查 modprobe 配置文件内容：Compute 模式会黑名单 nvidia-drm 但不黑名单 nvidia 核心
            let has_compute_modprobe = Path::new(MODPROBE_GPU_PATH).exists()
                && fs::read_to_string(MODPROBE_GPU_PATH)
                    .map(|c| c.contains("blacklist nvidia-drm") && !c.contains("alias nvidia off"))
                    .unwrap_or(false);
            if (!nvidia_drm_loaded && prime_mode == "off") || has_compute_modprobe {
                super::GpuMode::Compute
            } else {
                super::GpuMode::Hybrid
            }
        } else if nvidia_loaded {
            super::GpuMode::Nvidia
        } else {
            super::GpuMode::Integrated
        }
    }

    /// 检测 GPU 信息（通过 sysfs 直接读取，无需 lspci）
    ///
    /// # 参数
    /// * `force_detect` - 强制从 sysfs 检测而非从缓存读取
    ///
    /// # 返回
    /// `(nvidia_pci_bus_id, igpu_vendor)`
    pub fn detect_gpu_info(force_detect: bool) -> Result<(String, IgpuVendor), FtoolError> {
        debug!("开始检测 GPU 信息 (force_detect: {})", force_detect);

        let (nvidia_gpus, amd_gpus, intel_gpus) = Self::detect_all_gpus()?;

        let nvidia_pci_bus = if !nvidia_gpus.is_empty() {
            // 取第一个 NVIDIA GPU 的 BusID
            let raw_id = &nvidia_gpus[0].pci_id;
            let bus_id = format_pci_bus_id(raw_id)?;
            debug!("发现 NVIDIA GPU; raw={}, formatted={}", raw_id, bus_id);
            Some(bus_id)
        } else {
            None
        };

        let igpu_vendor = if !intel_gpus.is_empty() {
            debug!("发现 Intel 集显");
            Some(IgpuVendor::Intel)
        } else if !amd_gpus.is_empty() {
            debug!("发现 AMD 集显");
            Some(IgpuVendor::Amd)
        } else {
            None
        };

        let pci_bus = if force_detect {
            nvidia_pci_bus.ok_or_else(|| {
                warn!("强制检测模式下未找到 NVIDIA 显卡");
                FtoolError::Gpu("未找到 NVIDIA 显卡，请先切换至 hybrid 模式！".into())
            })
        } else {
            // 优先从缓存读取
            match super::cache::GpuCache::read() {
                Ok(data) => {
                    info!(
                        "从缓存读取 NVIDIA PCI 地址; cached_bus={}",
                        data.nvidia_gpu_pci_bus
                    );
                    Ok(data.nvidia_gpu_pci_bus)
                }
                Err(_) => {
                    warn!("缓存读取失败，尝试从 sysfs 获取");
                    nvidia_pci_bus
                        .ok_or_else(|| FtoolError::Gpu("无缓存数据且未找到 NVIDIA 显卡".into()))
                }
            }
        }?;

        let vendor = igpu_vendor.ok_or_else(|| {
            warn!("未能检测到集成显卡厂商");
            FtoolError::Gpu("无法检测集成显卡厂商".into())
        })?;

        Ok((pci_bus, vendor))
    }

    /// 获取 NVIDIA GPU 的 PCI Bus ID（格式化后的 "PCI:BB:DD:F" 格式）
    ///
    /// 当 `force_detect` 为 true 时直接从 sysfs 检测；
    /// 为 false 时优先从缓存读取，缓存不可用时报错。
    pub fn get_nvidia_pci_bus(force_detect: bool) -> Result<String, FtoolError> {
        if !force_detect {
            if let Ok(data) = super::cache::GpuCache::read() {
                return Ok(data.nvidia_gpu_pci_bus);
            }
            return Err(FtoolError::Gpu(
                "无缓存数据。此操作要求系统处于 hybrid 模式以创建缓存".into(),
            ));
        }

        Self::detect_gpu_info(true).map(|(bus, _)| bus)
    }

    /// 获取 NVIDIA GPU 的原始 PCI 设备 ID（如 "0000:01:00.0"），用于运行时电源控制
    pub fn get_nvidia_raw_pci_id() -> Result<String, FtoolError> {
        let (nvidia_gpus, _, _) = Self::detect_all_gpus()?;
        nvidia_gpus
            .first()
            .map(|gpu| gpu.pci_id.clone())
            .ok_or_else(|| FtoolError::Gpu("未找到 NVIDIA 显卡".into()))
    }

    /// 检测 NVIDIA GPU 是否在线（sysfs 中至少存在一个 NVIDIA 显示设备）
    pub fn is_nvidia_online() -> bool {
        let (nvidia_gpus, _, _) = Self::detect_all_gpus().unwrap_or_default();
        !nvidia_gpus.is_empty()
    }



    /// 检测系统挂起模式：S0ix (s2idle) 或 S3 (deep)
    pub fn detect_sleep_mode() -> SleepMode {
        let mem_sleep = fs::read_to_string("/sys/power/mem_sleep").unwrap_or_default();
        if mem_sleep.contains("[s2idle]") {
            debug!("检测到 S0ix (s2idle) 挂起模式");
            SleepMode::S0ix
        } else {
            debug!("检测到 S3 (deep) 挂起模式");
            SleepMode::S3
        }
    }

    /// 检测当前 NVIDIA GPU 是否支持运行时电源管理（runtime PM）
    ///
    /// 参考 system76-power 的实现：读取 NVIDIA 驱动附带的
    /// `/usr/share/doc/nvidia-driver-*/supported-gpus.json`，
    /// 查找当前 GPU 设备 ID 对应的条目，判断 features 中是否包含 "runtimepm"。
    ///
    /// 若无法确定（如 JSON 不存在或设备未找到），返回 Ok(false)。
    pub fn gpu_supports_runtimepm() -> Result<bool, FtoolError> {
        let (nvidia_gpus, _, _) = Self::detect_all_gpus()?;
        if nvidia_gpus.is_empty() {
            return Ok(false);
        }

        let device_id = nvidia_gpus[0].device_id;
        let nvidia_dev = Self::get_nvidia_device(device_id)?;
        info!(
            "NVIDIA 设备 0x{:04x} 特性: {:?}",
            device_id, nvidia_dev.features
        );
        Ok(nvidia_dev.features.iter().any(|f| f == "runtimepm"))
    }

    /// 从 supported-gpus.json 中查找指定设备 ID 对应的 NVIDIA GPU 条目
    ///
    /// 支持系统中存在多个支持的 JSON 文件版本（如旧版驱动残留），
    /// 遍历所有文件直至找到匹配的设备并返回其特性。
    fn get_nvidia_device(id: u16) -> Result<NvidiaDevice, FtoolError> {
        let supported_gpus: Vec<PathBuf> = fs::read_dir("/usr/share/doc")
            .map_err(|e| FtoolError::Gpu(format!("读取 /usr/share/doc 失败: {}", e)))?
            .filter_map(Result::ok)
            .map(|f| f.path())
            .filter(|f| f.to_str().unwrap_or_default().contains("nvidia-driver-"))
            .map(|f| f.join("supported-gpus.json"))
            .filter(|f| f.exists())
            .collect();

        if supported_gpus.is_empty() {
            return Err(FtoolError::Gpu(
                "未找到 supported-gpus.json（NVIDIA 驱动可能未安装）".into(),
            ));
        }

        for json_path in &supported_gpus {
            let raw = match fs::read_to_string(json_path) {
                Ok(s) => s,
                Err(e) => {
                    warn!("读取 {} 失败，跳过; error={}", json_path.display(), e);
                    continue;
                }
            };
            let gpus: SupportedGpus = match serde_json::from_str(&raw) {
                Ok(g) => g,
                Err(e) => {
                    warn!("解析 {} 失败，跳过; error={}", json_path.display(), e);
                    continue;
                }
            };
            for dev in gpus.chips {
                let did = dev.devid.trim_start_matches("0x").trim();
                if let Ok(parsed) = u16::from_str_radix(did, 16)
                    && parsed == id
                {
                    return Ok(dev);
                }
            }
        }

        let paths: Vec<String> = supported_gpus.iter().map(|p| p.display().to_string()).collect();
        Err(FtoolError::Gpu(format!(
            "在所有 supported-gpus.json ({}) 中均未找到设备 0x{:04x}",
            paths.join(", "),
            id
        )))
    }

    /// 获取 DMI 厂商字符串（如 "System76"、"LENOVO" 等）
    pub fn get_vendor_string() -> String {
        fs::read_to_string("/sys/class/dmi/id/sys_vendor")
            .unwrap_or_default()
            .trim()
            .to_string()
    }

    /// 获取 DMI 产品版本字符串（如 "oryp6"、"bonw15" 等）
    pub fn get_product_string() -> String {
        fs::read_to_string("/sys/class/dmi/id/product_version")
            .unwrap_or_default()
            .trim()
            .to_string()
    }

    /// 检测外接显示器是否需要 NVIDIA 独显驱动
    ///
    /// 参考 system76-power：某些机型的外接显示器物理连接在 NVIDIA GPU 上，
    /// 必须使用 NVIDIA 驱动才能正常输出。此方法通过 DMI 产品型号判断。
    pub fn is_external_display_requires_nvidia() -> Result<bool, FtoolError> {
        if !Self::can_switch()? {
            return Err(FtoolError::Gpu(
                "此设备不支持 GPU 切换，无法判断外接显示器需求".into(),
            ));
        }

        let model = fs::read_to_string("/sys/class/dmi/id/product_version")
            .map_err(|e| FtoolError::Gpu(format!("读取产品版本失败: {}", e)))?;

        Ok(EXTERNAL_DISPLAY_REQUIRES_NVIDIA.contains(&model.trim()))
    }

    /// 根据硬件和驱动特性推荐默认 GPU 模式
    ///
    /// 参考 system76-power 的 `get_default_graphics` 逻辑：
    /// - 非 System76 品牌 → Nvidia（保守策略）
    /// - 特定型号（如 bonw16）→ Nvidia
    /// - 支持 runtime PM → Hybrid（可按需切换，兼顾功耗）
    /// - 不支持 runtime PM → Integrated（避免 NVIDIA 空转耗电）
    pub fn get_default_graphics() -> Result<super::GpuMode, FtoolError> {
        if !Self::can_switch()? {
            return Err(FtoolError::Gpu("此设备不支持 GPU 切换".into()));
        }

        let vendor = Self::get_vendor_string();
        let product = Self::get_product_string();

        let runtimepm = match Self::gpu_supports_runtimepm() {
            Ok(ok) => ok,
            Err(err) => {
                warn!("无法判断 GPU runtimepm 支持: {}", err);
                false
            }
        };

        // 非 System76 品牌或特定型号默认使用独显
        if vendor != "System76" || DEFAULT_DISCRETE_MODELS.contains(&product.as_str()) {
            Ok(super::GpuMode::Nvidia)
        } else if runtimepm {
            Ok(super::GpuMode::Hybrid)
        } else {
            Ok(super::GpuMode::Integrated)
        }
    }
}

/// 格式化 PCI Bus ID：将 "0000:01:00.0" 转为 "PCI:1:0:0"
///
/// 注意：PCI 域（Domain）前缀在转换中被丢弃，因为 Xorg 配置中的
/// BusID 不需要域信息（`BusID "PCI:1:0:0"` 格式），且非零域在实际
/// 硬件中极为罕见。如果需要域感知，应扩展此函数。
///
/// # 错误
/// 当输入格式不符合预期时返回 `FtoolError::Gpu`。
fn format_pci_bus_id(raw: &str) -> Result<String, FtoolError> {
    // 格式: DDDD:BB:DD.F，移除域前缀（冒号前的内容）
    let without_domain = raw.split_once(':').map(|(_, rest)| rest).unwrap_or(raw);
    let parts: Vec<&str> = without_domain.split(':').collect();
    if parts.len() != 2 {
        return Err(FtoolError::Gpu(format!(
            "PCI 设备 ID 格式异常，无法分割 BDF: {raw}"
        )));
    }
    let bus = u32::from_str_radix(parts[0], 16)
        .map_err(|_| FtoolError::Gpu(format!("PCI Bus 解析失败: {} (raw: {raw})", parts[0])))?;
    let dev_func: Vec<&str> = parts[1].split('.').collect();
    if dev_func.len() != 2 {
        return Err(FtoolError::Gpu(format!(
            "PCI 设备 ID 格式异常，无法分割 dev.func: {raw}"
        )));
    }
    let dev = u32::from_str_radix(dev_func[0], 16)
        .map_err(|_| FtoolError::Gpu(format!("PCI Dev 解析失败: {} (raw: {raw})", dev_func[0])))?;
    let func = u32::from_str_radix(dev_func[1], 16)
        .map_err(|_| FtoolError::Gpu(format!("PCI Func 解析失败: {} (raw: {raw})", dev_func[1])))?;
    Ok(format!("PCI:{}:{}:{}", bus, dev, func))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_pci_bus_id_standard() {
        assert_eq!(format_pci_bus_id("0000:01:00.0").unwrap(), "PCI:1:0:0");
        assert_eq!(format_pci_bus_id("0000:0a:00.0").unwrap(), "PCI:10:0:0");
        assert_eq!(format_pci_bus_id("0000:03:02.1").unwrap(), "PCI:3:2:1");
    }

    #[test]
    fn test_format_pci_bus_id_edge_cases() {
        // 高 bus 号
        assert_eq!(format_pci_bus_id("0000:ff:1f.7").unwrap(), "PCI:255:31:7");
        // 无需 trim "0000:" 前缀
        assert_eq!(format_pci_bus_id("0001:01:00.0").unwrap(), "PCI:1:0:0");
    }

    #[test]
    fn test_format_pci_bus_id_invalid() {
        assert!(format_pci_bus_id("invalid").is_err());
        assert!(format_pci_bus_id("").is_err());
        assert!(format_pci_bus_id("0000:01:00").is_err());
        assert!(format_pci_bus_id("0000:xx:00.0").is_err());
    }
}
