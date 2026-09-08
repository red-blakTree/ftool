use crate::core::FtoolError;
use crate::features::gpu::constants::*;
use log::{debug, error, info, warn};
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
    /// 无法检测（mem_sleep 文件不存在或格式异常）
    Unknown,
}

/// GPU 设备信息（通过 sysfs 检测）
#[derive(Debug)]
struct GpuInfo {
    /// PCI 设备 ID，如 "0000:01:00.0"
    pci_id: String,
    /// PCI 设备号，用于匹配 supported-gpus.json 中的 devid
    device_id: u16,
    /// 子系统设备 ID（sysfs subsystem_device；0 = 读取失败/缺失）
    subdevice_id: u16,
    /// 子系统厂商 ID（sysfs subsystem_vendor；0 = 读取失败/缺失）
    subvendor_id: u16,
}

/// NVIDIA GPU 设备条目（对应 supported-gpus.json 中的 chips 条目）
///
/// 参考 system76-power 的实现，用于解析 NVIDIA 驱动附带的
/// `/usr/share/doc/nvidia-driver-*/supported-gpus.json` 文件。
#[derive(Debug, Clone, Deserialize)]
struct NvidiaDevice {
    /// 设备 ID（十六进制字符串，如 "0x1E90"）
    devid: String,
    /// 子设备 ID（十六进制字符串，如 "0x1E91"）——与 sysfs subsystem_device 精确匹配
    subdeviceid: Option<String>,
    /// 子厂商 ID（十六进制字符串，如 "0x1462"）——与 sysfs subsystem_vendor 精确匹配
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

/// 不可切换的 DMI chassis type（台式机/服务器等固定设备）
/// 参考 Desktop Management Interface (DMI) Specification：
/// 3=Desktop, 4=Low Profile Desktop, 5=Pizza Box, 6=Mini Tower,
/// 7=Tower, 17=Main Server Chassis, 23=Blade Server
const NON_SWITCHABLE_CHASSIS_TYPES: &[u32] = &[3, 4, 5, 6, 7, 17, 23];

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
    /// 显式触发一次 PCI bus rescan
    ///
    /// 仅在模式切换等确实需要重新枚举被移除设备的路径中调用，
    /// 普通查询/检测不得调用，避免产生热插拔等系统副作用。
    pub fn rescan_pci_bus() -> Result<(), FtoolError> {
        let rescan_path = Path::new("/sys/bus/pci/rescan");
        if !rescan_path.exists() {
            return Ok(());
        }
        fs::write(rescan_path, "1").map_err(|e| FtoolError::Gpu(format!("PCI rescan 失败: {}", e)))
    }

    /// 读取 sysfs 十六进制属性（class/vendor/device）并解析为 u32
    ///
    /// 单个属性读取失败/缺失时记录 debug 并返回 0，避免因个别 sysfs 属性
    /// 缺失而中断整个检测流程。
    fn read_sysfs_attr_u32(dev_path: &Path, attr: &str, dev_name: &str) -> u32 {
        match fs::read_to_string(dev_path.join(attr)) {
            Ok(s) => match u32::from_str_radix(s.trim().trim_start_matches("0x"), 16) {
                Ok(v) => v,
                Err(e) => {
                    // 内容存在但解析失败（非十六进制/超范围）：与 IO 失败同等对待，
                    // 记日志并返回 0，避免 0 值被静默当作合法 vendor/class 参与判断
                    debug!(
                        "解析 sysfs 属性内容失败; device={}, attr={}, content={:?}, error={}",
                        dev_name, attr, s, e
                    );
                    0
                }
            },
            Err(e) => {
                debug!(
                    "读取 sysfs 属性失败; device={}, attr={}, error={}",
                    dev_name, attr, e
                );
                0
            }
        }
    }

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

        // 先收集全部设备名再统一遍历（避免迭代期间目录变化），随后逐个筛选显示控制器
        let all_devices: Vec<String> = entries
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
            .collect();

        for dev_name in &all_devices {
            let dev_path = pci_path.join(dev_name);

            // 只关注显示控制器（class 0x03xxxx）
            let class = Self::read_sysfs_attr_u32(&dev_path, "class", dev_name);
            if class >> 16 != 0x03 {
                continue;
            }

            let vendor_id = Self::read_sysfs_attr_u32(&dev_path, "vendor", dev_name) as u16;
            let device_id = Self::read_sysfs_attr_u32(&dev_path, "device", dev_name) as u16;

            let gpu = GpuInfo {
                pci_id: dev_name.clone(),
                device_id,
                // 子系统 ID 用于 supported-gpus.json 的 SKU 级精确匹配；读取失败
                // 时返回 0，匹配逻辑按"信息不足"回退到 devid 级
                subdevice_id: Self::read_sysfs_attr_u32(&dev_path, "subsystem_device", dev_name)
                    as u16,
                subvendor_id: Self::read_sysfs_attr_u32(&dev_path, "subsystem_vendor", dev_name)
                    as u16,
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
        let chassis_type: u32 = chassis.trim().parse().unwrap_or(0);
        if chassis_type != 0 && NON_SWITCHABLE_CHASSIS_TYPES.contains(&chassis_type) {
            debug!(
                "检测到非笔记本机型 (chassis_type={})，不支持 GPU 切换",
                chassis_type
            );
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

        // sysfs 中未检测到 NVIDIA 显卡（可能在 Integrated 模式下被 udev 移除），
        // 通过有效缓存或完整的 Integrated 残留配置作为后备判断依据。
        // 缓存须通过内容校验（版本 + PCI 格式），仅"文件存在"不足以证明硬件可切换；
        // modprobe 残留须与 udev 移除规则同时存在（两者共同构成 Integrated 残留态），
        // 避免在缓存损坏或 reset 清理后仍误报可切换。
        if has_igpu {
            if crate::features::gpu::cache::GpuCache::read().is_ok() {
                info!(
                    "sysfs 未检测到 NVIDIA 但缓存有效，系统仍支持 GPU 切换（基于缓存判断，若已移除 NVIDIA 硬件请执行 ftool -g reset）"
                );
                return Ok(true);
            }
            if Path::new(MODPROBE_GPU_PATH).exists() && Path::new(UDEV_INTEGRATED_PATH).exists() {
                info!(
                    "sysfs 未检测到 NVIDIA 但存在 Integrated 残留配置，系统仍支持 GPU 切换（残留配置依据可能过期，若已移除 NVIDIA 硬件请执行 ftool -g reset）"
                );
                return Ok(true);
            }
        }

        if has_igpu && !has_nvidia {
            // 后备依据也缺失时给出可执行的恢复提示（如刚执行过 reset 导致缓存与
            // 配置均被清理、而 NVIDIA 已被 udev 移除尚未重新枚举的场景）
            warn!(
                "sysfs 未检测到 NVIDIA 且无有效缓存/残留配置；如确信存在 NVIDIA GPU，\
                 可先执行 PCI rescan 恢复设备枚举后再试"
            );
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

        let prime_mode = Self::read_prime_mode();
        let is_integrated = Self::has_integrated_config();

        Self::classify_mode(nvidia_loaded, &prime_mode, is_integrated)
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

    /// 读取 PRIME 离散模式标志文件内容
    ///
    /// 文件缺失视为无标记（首次使用/尚未切换过，属正常状态）；
    /// 文件存在但内容不在 {on, on-demand, off} 白名单内时给出警告，
    /// 避免后续模式分类在静默状态下基于异常内容做出错误判断。
    fn read_prime_mode() -> String {
        match fs::read_to_string(PRIME_DISCRETE_PATH) {
            Ok(content) => {
                let mode = content.trim().to_string();
                if matches!(mode.as_str(), "on" | "on-demand" | "off") {
                    mode
                } else {
                    warn!(
                        "{} 内容异常 {:?}，已按无标记处理，模式分类可能不准确",
                        PRIME_DISCRETE_PATH, mode
                    );
                    String::new()
                }
            }
            Err(_) => String::new(),
        }
    }

    /// 检查是否存在 Integrated 模式的 udev 配置
    fn has_integrated_config() -> bool {
        Path::new(MODPROBE_GPU_PATH).exists() && Path::new(UDEV_INTEGRATED_PATH).exists()
    }

    /// 综合分类当前 GPU 模式（纯决策函数，不涉及 I/O，便于测试）
    ///
    /// 决策优先级（按顺序）：
    /// 1. NVIDIA 模块未加载 → Integrated
    /// 2. prime-discrete 显式标记 "on" → Nvidia
    /// 3. prime-discrete="on-demand" → Hybrid
    /// 4. NVIDIA 已加载但无特征配置 → Nvidia（保守兜底）
    ///
    /// 注意：MODESET_PATH 同时被 Hybrid/Nvidia 两种模式写入（mod.rs 中两者都会
    /// 生成 nvidia-drm modeset=1 配置），不能作为 Hybrid 的判定特征；prime 标记
    /// 缺失/异常时保守归为 Nvidia，确保 power-off 门禁不会在 NVIDIA 驱动显示
    /// 输出时被误判为 Hybrid 而放行。
    ///
    /// prime-discrete="off"（切换到 Integrated 时写入的合法值）同样没有显式
    /// 分支：切换后未重启的过渡窗口内 nvidia 模块仍加载而 prime 已标记 off，
    /// 与标记缺失一样落入保守兜底归为 Nvidia——该窗口内 GPU 确实仍被驱动
    /// 使用，保守归 Nvidia 才能让 power-off 门禁不放行；重启后模块卸载自然
    /// 归为 Integrated。此为保守安全设计，不是缺陷。
    fn classify_mode(nvidia_loaded: bool, prime_mode: &str, is_integrated: bool) -> super::GpuMode {
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

        // prime-discrete="on-demand" → Hybrid 模式
        // （MODESET_PATH 不能作为 Hybrid 特征：Nvidia 模式同样写入该文件）
        if prime_mode == "on-demand" {
            return super::GpuMode::Hybrid;
        }

        // NVIDIA 已加载但无特征配置 → Nvidia（保守兜底）
        super::GpuMode::Nvidia
    }

    /// 获取 NVIDIA GPU 的原始 PCI 设备 ID（如 "0000:01:00.0"），用于运行时电源控制
    pub fn get_nvidia_raw_pci_id() -> Result<String, FtoolError> {
        let (mut nvidia_gpus, _, _) = Self::detect_all_gpus()?;
        // read_dir 枚举顺序无保证：按 PCI 地址排序（pci_id 为定宽字符串
        // "DDDD:BB:DD.F"，字典序即总线序）取首个设备，与 gpu_supports_runtimepm
        // 的选择保持一致，避免多 NVIDIA 设备（内置 dGPU + eGPU）时两个函数
        // 因枚举顺序不同而选中不同的 GPU
        nvidia_gpus.sort_by(|a, b| a.pci_id.cmp(&b.pci_id));
        nvidia_gpus
            .first()
            .map(|gpu| gpu.pci_id.clone())
            .ok_or_else(|| FtoolError::Gpu("未找到 NVIDIA 显卡".into()))
    }

    /// 检测 NVIDIA GPU 是否在线（sysfs 中至少存在一个 NVIDIA 显示设备）
    ///
    /// 检测失败时不静默：输出 error 日志并按"不在线"返回。runtime_power_off
    /// 取不到 PCI ID 后会依赖本函数的 false 幂等放行（视为已关闭），error
    /// 日志用于把"检测失败"与"确实已关闭"区分开；调用方在返回值上仍无法
    /// 区分二者，如需严格区分须把签名改为返回 Result（涉及 power.rs 等调用
    /// 方一并调整），留待后续。
    pub fn is_nvidia_online() -> bool {
        match Self::detect_all_gpus() {
            Ok((nvidia_gpus, _, _)) => !nvidia_gpus.is_empty(),
            Err(e) => {
                error!("检测 NVIDIA GPU 在线状态失败，按不在线处理: {}", e);
                false
            }
        }
    }

    /// 检测系统挂起模式：S0ix (s2idle) 或 S3 (deep)
    pub fn detect_sleep_mode() -> SleepMode {
        let mem_sleep = fs::read_to_string("/sys/power/mem_sleep").unwrap_or_default();
        if mem_sleep.contains("[s2idle]") {
            debug!("检测到 S0ix (s2idle) 挂起模式");
            return SleepMode::S0ix;
        }
        if mem_sleep.contains("[deep]") {
            debug!("检测到 S3 (deep) 挂起模式");
            return SleepMode::S3;
        }
        // mem_sleep 存在但没有方括号默认值（异常情况）
        if mem_sleep.contains("s2idle") {
            debug!("检测到 S0ix (s2idle) 挂起模式（无方括号标记）");
            return SleepMode::S0ix;
        }
        if mem_sleep.contains("deep") {
            debug!("检测到 S3 (deep) 挂起模式（无方括号标记）");
            return SleepMode::S3;
        }
        debug!(
            "无法从 /sys/power/mem_sleep 检测休眠模式，内容: '{}'",
            mem_sleep.trim()
        );
        SleepMode::Unknown
    }

    /// 检测当前 NVIDIA GPU 是否支持运行时电源管理（runtime PM）
    ///
    /// 参考 system76-power 的实现：读取 NVIDIA 驱动附带的
    /// `/usr/share/doc/nvidia-driver-*/supported-gpus.json`，
    /// 查找当前 GPU 设备 ID 对应的条目，判断 features 中是否包含 "runtimepm"。
    ///
    /// 若无法确定（如 supported-gpus.json 缺失或未收录该设备），返回错误，
    /// 由调用方决定降级策略（如按不支持 runtime PM 处理）。
    pub fn gpu_supports_runtimepm() -> Result<bool, FtoolError> {
        let (nvidia_gpus, _, _) = Self::detect_all_gpus()?;
        if nvidia_gpus.is_empty() {
            return Ok(false);
        }

        // read_dir 枚举顺序无保证：按 PCI 地址排序（固定宽度字符串，字典序即
        // 总线顺序）取首个设备，避免多 NVIDIA 设备（内置 dGPU + eGPU）时误判
        let mut gpus = nvidia_gpus;
        gpus.sort_by(|a, b| a.pci_id.cmp(&b.pci_id));
        let gpu = &gpus[0];
        let nvidia_dev = Self::get_nvidia_device(gpu)?;
        info!(
            "NVIDIA 设备 {} (0x{:04x}) 特性: {:?}",
            gpu.pci_id, gpu.device_id, nvidia_dev.features
        );
        Ok(nvidia_dev.features.iter().any(|f| f == "runtimepm"))
    }

    /// 从 supported-gpus.json 中查找指定 GPU 对应的 NVIDIA 设备条目
    ///
    /// 支持系统中存在多个支持的 JSON 文件版本（如旧版驱动残留），
    /// 遍历所有文件直至找到匹配的设备并返回其特性。
    fn get_nvidia_device(gpu: &GpuInfo) -> Result<NvidiaDevice, FtoolError> {
        let supported_gpus: Vec<PathBuf> = fs::read_dir("/usr/share/doc")
            .map_err(|e| FtoolError::Gpu(format!("读取 /usr/share/doc 失败: {}", e)))?
            .filter_map(Result::ok)
            .map(|f| f.path())
            // 兼容不同发行版的驱动文档目录命名：Ubuntu/Pop!_OS 的 nvidia-driver-<ver>，
            // Fedora rpmfusion 的 xorg-x11-drv-nvidia-<ver>（是否附带该 JSON 以目标
            // 环境实测为准，此处至少保证两种布局都能被扫描到）
            .filter(|f| {
                let name = f.to_str().unwrap_or_default();
                name.contains("nvidia-driver-") || name.contains("xorg-x11-drv-nvidia")
            })
            .map(|f| f.join("supported-gpus.json"))
            .filter(|f| f.exists())
            .collect();

        if supported_gpus.is_empty() {
            return Err(FtoolError::Gpu(
                "未找到 supported-gpus.json（NVIDIA 驱动可能未安装）".into(),
            ));
        }

        // 同一 devid 在 JSON 中常有多个条目（公版 + 各厂商 SKU，以 subdeviceid/
        // subvendorid 区分），各条目 features 可能不同（直接影响 runtimepm 判断）：
        // devid 命中后优先返回与 sysfs 实测子设备信息精确匹配的条目；
        // 无精确条目时回退到首个 devid 命中（兼容 JSON 未携带子设备信息、
        // 或 sysfs 读不到子系统 ID 的旧硬件/旧格式）
        let mut fallback: Option<NvidiaDevice> = None;
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
                    && parsed == gpu.device_id
                {
                    if Self::subsystem_matches(&dev, gpu) {
                        info!(
                            "supported-gpus.json 子设备精确匹配; pci={}, devid=0x{:04x}",
                            gpu.pci_id, gpu.device_id
                        );
                        return Ok(dev);
                    }
                    if fallback.is_none() {
                        fallback = Some(dev);
                    }
                }
            }
        }
        if let Some(dev) = fallback {
            return Ok(dev);
        }

        let paths: Vec<String> = supported_gpus
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        Err(FtoolError::Gpu(format!(
            "在所有 supported-gpus.json ({}) 中均未找到设备 0x{:04x}",
            paths.join(", "),
            gpu.device_id
        )))
    }

    /// 判断 JSON 条目的 subdeviceid/subvendorid 是否与 sysfs 实测值精确一致
    ///
    /// 条目缺任一子 ID、或 sysfs 侧读取失败（值为 0）时不构成精确匹配——
    /// 信息不足时回退到 devid 级匹配，避免把错误 SKU 条目当作本机硬件。
    fn subsystem_matches(dev: &NvidiaDevice, gpu: &GpuInfo) -> bool {
        let (Some(json_subdev), Some(json_subvend)) = (&dev.subdeviceid, &dev.subvendorid) else {
            return false;
        };
        if gpu.subdevice_id == 0 || gpu.subvendor_id == 0 {
            return false;
        }
        let parse = |s: &str| u16::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok();
        matches!(
            (parse(json_subdev), parse(json_subvend)),
            (Some(d), Some(v)) if d == gpu.subdevice_id && v == gpu.subvendor_id
        )
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

        // 已知行为（自证闭环，刻意不改）：System76 机器已处于 Integrated 时
        // NVIDIA 已被 udev 移除、sysfs 检测不到 GPU，gpu_supports_runtimepm
        // 返回 false → 这里推荐 Integrated。即"检测不到 GPU 便无法证明支持
        // runtimepm"，对已 Integrated 的机器只会重复推荐 Integrated、不会
        // 凭空建议 Hybrid（保守安全方向）；若要打破闭环，需在 GPU 不在线时
        // 借助缓存中的设备 ID 查询 supported-gpus.json，留待后续。
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
    /// 获取所有 NVIDIA PCI 设备的 (vendor, device) 对（所有功能号，不限于显示控制器）
    /// 用于在 GPU 仍然在线时保存设备 ID 到缓存，供 PCIe 断电后恢复使用
    pub fn get_all_nvidia_device_ids()
    -> Result<Vec<crate::features::gpu::cache::NvidiaDeviceId>, FtoolError> {
        let pci_path = Path::new("/sys/bus/pci/devices");
        if !pci_path.is_dir() {
            return Err(FtoolError::Gpu("/sys/bus/pci/devices 不存在".into()));
        }
        let entries = fs::read_dir(pci_path)
            .map_err(|e| FtoolError::Gpu(format!("读取 PCI 目录失败: {}", e)))?;

        let mut ids = Vec::new();
        for entry in entries.flatten() {
            let vendor = match fs::read_to_string(entry.path().join("vendor")) {
                Ok(s) => u16::from_str_radix(s.trim_start_matches("0x"), 16).unwrap_or(0),
                Err(e) => {
                    debug!(
                        "读取 PCI vendor 失败，跳过; path={}, error={}",
                        entry.path().display(),
                        e
                    );
                    continue;
                }
            };
            if vendor != 0x10DE {
                continue;
            }
            // 显式处理读取/解析失败：瞬时错误静默吞掉会把 (0x10de, 0x0000)
            // 这类无效项写入缓存，供后续 PCIe 断电恢复逻辑使用时产生误匹配
            let device = match fs::read_to_string(entry.path().join("device")) {
                Ok(s) => match u16::from_str_radix(s.trim_start_matches("0x"), 16) {
                    Ok(d) if d != 0 => d,
                    Ok(_) => {
                        warn!(
                            "NVIDIA 设备 ID 解析为 0，跳过无效项; path={}",
                            entry.path().display()
                        );
                        continue;
                    }
                    Err(_) => {
                        warn!(
                            "NVIDIA 设备 ID 解析失败，跳过; path={}, value={}",
                            entry.path().display(),
                            s.trim()
                        );
                        continue;
                    }
                },
                Err(e) => {
                    warn!(
                        "读取 NVIDIA 设备 ID 失败，跳过; path={}, error={}",
                        entry.path().display(),
                        e
                    );
                    continue;
                }
            };
            ids.push(crate::features::gpu::cache::NvidiaDeviceId { vendor, device });
        }

        if ids.is_empty() {
            return Err(FtoolError::Gpu("未找到任何 NVIDIA PCI 设备".into()));
        }
        Ok(ids)
    }
}

#[cfg(test)]
mod tests {
    use crate::features::gpu::GpuMode;

    fn classify(nvidia_loaded: bool, prime: &str, integrated: bool) -> GpuMode {
        super::GpuDetector::classify_mode(nvidia_loaded, prime, integrated)
    }

    /// NVIDIA 模块未加载（无论 prime 标记如何）→ Integrated
    #[test]
    fn classify_nvidia_not_loaded_is_integrated() {
        assert_eq!(classify(false, "", false), GpuMode::Integrated);
        assert_eq!(classify(false, "on", false), GpuMode::Integrated);
    }

    /// prime 标记 "on" → Nvidia
    #[test]
    fn classify_prime_on_is_nvidia() {
        assert_eq!(classify(true, "on", false), GpuMode::Nvidia);
        // 残留 Integrated 配置（is_integrated）不改变 prime 显式标记的裁决
        assert_eq!(classify(true, "on", true), GpuMode::Nvidia);
    }

    /// prime 标记 "on-demand" → Hybrid（modeset 配置不作为 Hybrid 特征：
    /// Nvidia 模式同样写入该文件，见 C1 修复）
    #[test]
    fn classify_prime_on_demand_is_hybrid() {
        assert_eq!(classify(true, "on-demand", false), GpuMode::Hybrid);
    }

    /// prime 标记缺失/异常（按空串处理）且 nvidia 已加载 → 保守归 Nvidia：
    /// 保证 power-off 门禁不会因误判 Hybrid 而在 NVIDIA 驱动显示输出时放行
    #[test]
    fn classify_prime_missing_with_nvidia_falls_back_to_nvidia() {
        assert_eq!(classify(true, "", false), GpuMode::Nvidia);
        assert_eq!(classify(true, "", true), GpuMode::Nvidia);
    }
}
