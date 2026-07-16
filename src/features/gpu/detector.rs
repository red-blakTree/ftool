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
        let modules = fs::read_to_string("/proc/modules").unwrap_or_default();
        let nvidia_loaded = Self::is_nvidia_module_loaded(&modules);
        let nouveau_loaded = Self::is_nouveau_module_loaded(&modules);
        let nvidia_drm_loaded = Self::is_nvidia_drm_module_loaded(&modules);

        let prime_mode = Self::read_prime_mode();
        let is_integrated = Self::has_integrated_config();
        let is_compute = Self::has_compute_modprobe_config();
        let has_modeset = Self::has_modeset_config();

        Self::classify_mode(
            nvidia_loaded,
            nouveau_loaded,
            nvidia_drm_loaded,
            &prime_mode,
            is_integrated,
            is_compute,
            has_modeset,
        )
    }

// ========== 模式分类辅助函数 ==========

/// 检查 /proc/modules 中是否加载了 NVIDIA 核心模块
fn is_nvidia_module_loaded(modules: &str) -> bool {
    modules.lines().any(|line| {
        let name = line.split_whitespace().next().unwrap_or("");
        matches!(
            name,
            "nvidia" | "nvidia_drm" | "nvidia_current" | "nvidia_current_drm"
        )
    })
}

/// 检查 /proc/modules 中是否加载了 NVIDIA DRM 模块
fn is_nvidia_drm_module_loaded(modules: &str) -> bool {
    modules.lines().any(|line| {
        let name = line.split_whitespace().next().unwrap_or("");
        matches!(name, "nvidia_drm" | "nvidia_current_drm")
    })
}

/// 检查 /proc/modules 中是否加载了 nouveau 开源驱动
fn is_nouveau_module_loaded(modules: &str) -> bool {
    modules.lines().any(|line| {
        let name = line.split_whitespace().next().unwrap_or("");
        name == "nouveau"
    })
}

/// 读取 PRIME 离散模式标志文件内容
fn read_prime_mode() -> String {
    fs::read_to_string(PRIME_DISCRETE_PATH)
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// 检查是否存在 Integrated 模式的 udev 配置
fn has_integrated_config() -> bool {
    Path::new(MODPROBE_GPU_PATH).exists() && Path::new(UDEV_INTEGRATED_PATH).exists()
}

/// 检查是否存在 modeset 配置文件
fn has_modeset_config() -> bool {
    Path::new(MODESET_PATH).exists()
}

/// 检查 modprobe 配置文件内容是否具有 Compute 模式特征
///（黑名单 nvidia-drm 但不黑名单 nvidia 核心模块）
fn has_compute_modprobe_config() -> bool {
    Path::new(MODPROBE_GPU_PATH).exists()
        && fs::read_to_string(MODPROBE_GPU_PATH)
            .map(|c| c.contains("blacklist nvidia-drm") && !c.contains("alias nvidia off"))
            .unwrap_or(false)
}

/// 综合分类当前 GPU 模式（纯决策函数，不涉及 I/O，便于测试）
///
/// 决策优先级（按顺序）：
/// 1. NVIDIA 模块未加载 → Integrated
/// 2. prime-discrete 显式标记 "on" → Nvidia
/// 3. prime-discrete="on-demand" 或有 modeset 配置 → Hybrid/Compute
/// 4. NVIDIA 已加载但无特征配置 → Nvidia（保守兜底）
fn classify_mode(
    nvidia_loaded: bool,
    _nouveau_loaded: bool,
    nvidia_drm_loaded: bool,
    prime_mode: &str,
    is_integrated: bool,
    is_compute: bool,
    has_modeset: bool,
) -> super::GpuMode {
    // NVIDIA 模块未加载 → Integrated
    // （无论 nouveau 是否加载、是否存在 Integrated 配置残留）
    if !nvidia_loaded {
        return super::GpuMode::Integrated;
    }

    // 以下所有分支 nvidia_loaded 均为 true

    // 配置/运行时不一致告警
    if is_integrated {
        warn!(
            "配置/运行时不匹配: 存在 Integrated 模式的 modprobe 黑名单配置 \
             但 nvidia 内核模块已加载（可能由其他软件或手动操作加载）"
        );
    }

    // prime-discrete 显式标记 "on" → Nvidia 模式
    if prime_mode == "on" {
        return super::GpuMode::Nvidia;
    }

    // prime-discrete="on-demand" 或有 modeset 配置 → 在 Hybrid/Compute 间区分
    if prime_mode == "on-demand" || has_modeset {
        // Compute 模式判定依据（二选一满足即可）：
        // (a) 存在 Compute 特征 modprobe 配置（黑名单 drm 但不黑名单核心模块）
        // (b) 用户显式设置 prime-discrete="off" 且 nvidia-drm 未加载
        let looks_like_compute = is_compute
            || (prime_mode == "off" && !nvidia_drm_loaded);

        return if looks_like_compute {
            super::GpuMode::Compute
        } else {
            super::GpuMode::Hybrid
        };
    }

    // NVIDIA 已加载但无特征配置 → Nvidia（保守兜底）
    super::GpuMode::Nvidia
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

