mod cache;
pub mod cli;
mod constants;
mod detector;
mod generator;
mod helper;

use crate::core::FtoolError;
use cache::CacheData;
use helper::{create_file, create_file_bytes};
use log::{info, warn};

/// GPU 工作模式枚举
///
/// 用于控制 NVIDIA Optimus 笔记本的显卡切换策略。
/// X11 环境下 Nvidia 模式会写入 PrimaryGPU 配置；
/// Wayland 下由 nvidia-drm modeset 接管显示输出。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuMode {
    /// 仅使用集成显卡，禁用所有 NVIDIA 内核模块
    Integrated,
    /// 集显输出画面，NVIDIA 仅用于 CUDA/计算任务
    Compute,
    /// PRIME 混合模式，按需动态渲染
    Hybrid,
    /// 仅使用 NVIDIA 独立显卡
    Nvidia,
}

impl GpuMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Integrated => "integrated",
            Self::Compute => "compute",
            Self::Hybrid => "hybrid",
            Self::Nvidia => "nvidia",
        }
    }
}

impl std::str::FromStr for GpuMode {
    type Err = FtoolError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "integrated" => Ok(Self::Integrated),
            "compute" => Ok(Self::Compute),
            "hybrid" => Ok(Self::Hybrid),
            "nvidia" => Ok(Self::Nvidia),
            _ => Err(FtoolError::Input(format!("不支持的模式: {}", s))),
        }
    }
}


/// NVIDIA 模式的专有配置选项
#[derive(Debug, Clone, Default)]
pub struct NvidiaOptions {
    /// Nvidia 模式的 Coolbits 位掩码（None = 不启用）
    pub coolbits: Option<u32>,
    /// RTD3 运行时电源管理级别（None = 不启用）
    pub rtd3: Option<u32>,
    /// 使用 nvidia-current 内核模块替代默认的 nvidia
    pub use_nvidia_current: bool,
}

/// GPU 模式切换选项（封装了目标模式和 NVIDIA 专有选项）
pub struct SwitchOptions {
    pub mode: GpuMode,
    pub nvidia_opts: NvidiaOptions,
}

/// 运行时电源切换操作类型
pub enum PowerAction {
    On,
    Off,
    Auto,
}

/// GPU 控制器 —— 提供显卡模式切换、电源管理、缓存管理等核心功能
pub struct GpuController;

impl GpuController {
    // ========== 公开 API ==========

    /// 切换 GPU 模式（需重启生效）
    pub fn switch_mode(opts: SwitchOptions) -> Result<(), FtoolError> {
        // 先检查系统是否支持 GPU 切换
        if !detector::GpuDetector::can_switch()? {
            return Err(FtoolError::Gpu(
                "此设备不支持 GPU 切换（可能为台式机或没有双显卡）".into(),
            ));
        }

        info!("🚀 正在切换到 {} 模式...", opts.mode.as_str());
        helper::cleanup()?;

        let nv = &opts.nvidia_opts;
        match opts.mode {
            GpuMode::Integrated => Self::switch_integrated()?,
            GpuMode::Compute => Self::switch_compute(nv.use_nvidia_current)?,
            GpuMode::Hybrid => Self::switch_hybrid(nv.rtd3, nv.use_nvidia_current)?,
            GpuMode::Nvidia => {
                Self::switch_nvidia(nv.coolbits, nv.use_nvidia_current)?;
            }
        }

        // 写入 PRIME 离散模式标志（参考 system76-power 的实现）
        let prime_mode = match opts.mode {
            GpuMode::Hybrid => "on-demand",
            GpuMode::Nvidia => "on",
            GpuMode::Compute | GpuMode::Integrated => "off",
        };
        helper::set_prime_discrete(prime_mode)?;

        // 追加挂起电源管理配置（非 Integrated 模式下需要）
        helper::append_sleep_config(opts.mode)?;

        // 重建 initramfs
        helper::rebuild_initramfs()?;
        info!("✅ 切换成功！请重启计算机以使更改生效。");
        Ok(())
    }

    /// 查询当前 GPU 模式
    pub fn query_mode() -> GpuMode {
        detector::GpuDetector::query_current_mode()
    }

    /// 检测系统是否支持 GPU 切换（笔记本 + 双显卡）
    pub fn can_switch() -> Result<bool, FtoolError> {
        detector::GpuDetector::can_switch()
    }

    /// 重置所有由 ftool 生成的 GPU 配置
    pub fn reset() -> Result<(), FtoolError> {
        info!("🔄 正在重置 GPU 配置...");
        helper::cleanup()?;
        cache::GpuCache::delete()?;
        helper::rebuild_initramfs()?;
        info!("✅ 重置成功！请重启计算机以使更改生效。");
        Ok(())
    }

    /// 创建 NVIDIA GPU 缓存（需处于 hybrid 或 compute 模式）
    pub fn cache_create() -> Result<(), FtoolError> {
        let mode = detector::GpuDetector::query_current_mode();
        if mode != GpuMode::Hybrid && mode != GpuMode::Compute {
            return Err(FtoolError::Input(
                "--cache-create 要求系统当前处于 hybrid 或 compute 模式".into(),
            ));
        }
        Self::write_nvidia_cache()
    }

    /// 删除 GPU 缓存
    pub fn delete_cache() -> Result<(), FtoolError> {
        cache::GpuCache::delete()
    }

    /// 查询 GPU 缓存内容
    pub fn cache_query() -> Result<String, FtoolError> {
        cache::GpuCache::query()
    }

    /// 运行时电源控制（无需重启，立即生效）
    pub fn power(action: PowerAction) -> Result<(), FtoolError> {
        match action {
            PowerAction::On => helper::runtime_power_on(),
            PowerAction::Off => helper::runtime_power_off(),
            PowerAction::Auto => helper::auto_power(),
        }
    }

    /// 查询运行时 NVIDIA GPU 电源状态
    pub fn query_power() -> bool {
        helper::query_runtime_power()
    }

    /// 根据硬件和驱动特性推荐默认 GPU 模式
    ///
    /// 参考 system76-power 的 `get_default_graphics` 逻辑：
    /// - 非 System76 品牌默认使用独显（保守策略）
    /// - 特定型号默认使用独显
    /// - 支持 runtime PM 的 GPU 默认 Hybrid
    /// - 不支持 runtime PM 的 GPU 默认 Integrated
    pub fn get_default() -> Result<GpuMode, FtoolError> {
        detector::GpuDetector::get_default_graphics()
    }

    /// 检测外接显示器是否需要 NVIDIA 独显驱动
    ///
    /// 某些机型的外接显示器物理连接在 NVIDIA GPU 上，
    /// 必须使用 NVIDIA 驱动才能正常输出。
    pub fn external_display_requires_nvidia() -> Result<bool, FtoolError> {
        detector::GpuDetector::is_external_display_requires_nvidia()
    }

    /// 检测当前 NVIDIA GPU 是否支持运行时电源管理
    pub fn supports_runtimepm() -> Result<bool, FtoolError> {
        detector::GpuDetector::gpu_supports_runtimepm()
    }

    // ========== 内部模式切换策略 ==========

    /// 写入 NVIDIA GPU PCI 地址缓存和设备 ID
    ///
    /// 优先通过 sysfs 检测当前 NVIDIA GPU 的 PCI 地址并写入缓存。
    /// GPU 在线时同时收集所有 NVIDIA 设备 ID（用于 PCIe 断电后恢复）。
    /// 如果 sysfs 中找不到（例如 Integrated 模式下 NVIDIA 已被 udev 移除），
    /// 则回退读取已有缓存数据并重新写入以保持缓存新鲜。
    fn write_nvidia_cache() -> Result<(), FtoolError> {
        let (pci_bus, device_ids) = match detector::GpuDetector::get_nvidia_raw_pci_id() {
            Ok(raw) => {
                // 将原始 DDDD:BB:DD.F 格式标准化为 "PCI:BB:DD:F" 写入缓存
                let without_domain = raw.split_once(':').map(|(_, r)| r).unwrap_or(&raw);
                let parts: Vec<&str> = without_domain.split(':').collect();
                if parts.len() != 2 {
                    return Err(FtoolError::Gpu(format!(
                        "PCI 设备 ID 格式异常: {}",
                        raw
                    )));
                }
                let dev_func: Vec<&str> = parts[1].split('.').collect();
                if dev_func.len() != 2 {
                    return Err(FtoolError::Gpu(format!(
                        "PCI 设备 ID 格式异常: {}",
                        raw
                    )));
                }
                let bus = u32::from_str_radix(parts[0], 16).map_err(|_| {
                    FtoolError::Gpu(format!("PCI Bus 解析失败: {}", raw))
                })?;
                let dev = u32::from_str_radix(dev_func[0], 16).map_err(|_| {
                    FtoolError::Gpu(format!("PCI Dev 解析失败: {}", raw))
                })?;
                let func = u32::from_str_radix(dev_func[1], 16).map_err(|_| {
                    FtoolError::Gpu(format!("PCI Func 解析失败: {}", raw))
                })?;
                let bus_str = format!("PCI:{}:{}:{}", bus, dev, func);

                // GPU 在线时同时收集所有 NVIDIA 设备 ID（用于 PCIe 断电后恢复）
                let ids: Vec<cache::NvidiaDeviceId> =
                    detector::GpuDetector::get_all_nvidia_device_ids()
                        .unwrap_or_default();
                (bus_str, ids)
            }
            Err(_) => {
                // Fallback: 尝试读取已有缓存
                let data = cache::GpuCache::read().map_err(|_| {
                    FtoolError::Gpu(
                        "sysfs 未检测到 NVIDIA 显卡且无缓存数据，无法保存缓存。".into(),
                    )
                })?;
                info!(
                    "sysfs 未检测到 NVIDIA，使用现有缓存中的 PCI 地址和设备 ID; bus={}",
                    data.nvidia_gpu_pci_bus
                );
                (data.nvidia_gpu_pci_bus, data.nvidia_device_ids)
            }
        };
        cache::GpuCache::write(&CacheData::new(pci_bus, device_ids))
    }

    /// 统一配置 NVIDIA GPU 相关 systemd 服务，消除 switch_* 中的重复代码
    ///
    /// 各模式对服务的需求：
    /// - Integrated:  全部禁用（persistenced=false, fallback=false, suspend=false）
    /// - Compute:     仅 persistenced（persistenced=true,  fallback=false, suspend=false）
    /// - Hybrid:      persistenced + suspend（persistenced=true,  fallback=false, suspend=true）
    /// - Nvidia:      全部启用（persistenced=true,  fallback=true,  suspend=true）
    fn configure_gpu_services(persistenced: bool, fallback: bool, suspend: bool) {
        let mut errors: Vec<String> = Vec::new();
        if let Err(e) = helper::toggle_service("nvidia-persistenced.service", persistenced) {
            errors.push(format!("nvidia-persistenced: {}", e));
        }
        if let Err(e) = helper::toggle_service("nvidia-fallback.service", fallback) {
            errors.push(format!("nvidia-fallback: {}", e));
        }
        if let Err(e) = helper::configure_nvidia_suspend_services(suspend) {
            errors.push(format!("suspend services: {}", e));
        }
        if !errors.is_empty() {
            warn!("GPU 服务配置失败: {}", errors.join("; "));
        }
    }

    /// Integrated 模式：完全禁用 NVIDIA 驱动，仅使用集成显卡
    fn switch_integrated() -> Result<(), FtoolError> {
        // 保存 NVIDIA GPU PCI 地址缓存，供后续从 Integrated 模式切换时使用
        // （核显模式的 udev 规则会物理移除 NVIDIA PCI 设备，缓存是唯一后备数据源）
        if let Err(e) = Self::write_nvidia_cache() {
            warn!("保存 NVIDIA GPU 缓存失败，继续执行; error={}", e);
        }

        Self::configure_gpu_services(false, false, false);

        // 写入 modprobe 黑名单（使用二进制写入避免编码问题）
        create_file_bytes(constants::MODPROBE_GPU_PATH, constants::MODPROBE_INTEGRATED)?;

        // 写入 udev 规则：自动移除 NVIDIA 设备
        create_file(
            constants::UDEV_INTEGRATED_PATH,
            constants::UDEV_INTEGRATED,
            false,
        )?;

        // 二次确认：cleanup() 已在 switch_mode 入口执行，此处为额外安全清理
        // 确保 modeset 配置文件不会在 Integrated 模式下残留
        if std::path::Path::new(constants::MODESET_PATH).exists() {
            let _ = std::fs::remove_file(constants::MODESET_PATH);
        }

        Ok(())
    }

    /// Compute 模式：集显输出画面，NVIDIA 可用于 CUDA 计算（参考 system76-power）
    fn switch_compute(use_nvidia_current: bool) -> Result<(), FtoolError> {
        Self::configure_gpu_services(true, false, false);

        // 写入 modprobe：仅黑名单显示相关模块，保留 nvidia 核心驱动供计算使用
        create_file_bytes(constants::MODPROBE_GPU_PATH, constants::MODPROBE_COMPUTE)?;

        // 写入 Compute 专用 modeset 配置（不启用 drm modeset，因为 nvidia-drm 已被黑名单）
        let modeset_content = if use_nvidia_current {
            constants::MODESET_COMPUTE_CURRENT_CONTENT
        } else {
            constants::MODESET_COMPUTE_CONTENT
        };
        create_file(constants::MODESET_PATH, modeset_content, false)?;

        // 写入 Compute 专用 udev 电源管理规则（保留 Audio/USB/UCSI 设备）
        create_file(
            constants::UDEV_PM_PATH,
            constants::UDEV_PM_COMPUTE_CONTENT,
            false,
        )?;

        Self::write_nvidia_cache()
    }

    /// Hybrid 模式：PRIME 按需渲染，支持 RTD3 动态电源管理
    fn switch_hybrid(rtd3: Option<u32>, use_nvidia_current: bool) -> Result<(), FtoolError> {
        Self::configure_gpu_services(true, false, true);

        // 写入空 modprobe 配置（允许所有驱动正常加载）
        create_file_bytes(constants::MODPROBE_GPU_PATH, constants::MODPROBE_EMPTY)?;

        // 写入 modeset 配置（含 RTD3 电源管理参数）
        let modeset_content =
            generator::ConfigGenerator::generate_modeset_content(rtd3, use_nvidia_current);
        create_file(constants::MODESET_PATH, &modeset_content, false)?;

        // 写入 Hybrid 专用 udev 电源管理规则（移除 Audio/USB/UCSI 设备以节省电量）
        create_file(constants::UDEV_PM_PATH, constants::UDEV_PM_CONTENT, false)?;

        Self::write_nvidia_cache()
    }

    /// Nvidia 模式：仅使用 NVIDIA 独立显卡输出画面
    ///
    /// 写入 X11 PrimaryGPU 配置使 NVIDIA 成为主显示器（参考 system76-power），
    /// 同时写入 nvidia-drm modeset=1 确保 Wayland 下的 DRM 直通输出。
    fn switch_nvidia(
        coolbits: Option<u32>,
        use_nvidia_current: bool,
    ) -> Result<(), FtoolError> {
        Self::configure_gpu_services(true, true, true);

        // 写入空 modprobe 配置
        create_file_bytes(constants::MODPROBE_GPU_PATH, constants::MODPROBE_EMPTY)?;

        // 写入 modeset 配置（含可选 Coolbits 参数）
        Self::write_modeset_config(use_nvidia_current, coolbits)?;

        // 写入 X11 PrimaryGPU 配置（参考 system76-power 的 discrete 模式）
        helper::write_xorg_nvidia_config()?;

        // 写入 NVIDIA 环境变量配置，确保应用使用 NVIDIA 渲染
        helper::write_nvidia_env_config()?;

        Ok(())
    }

    /// 写入 NVIDIA modeset 内核模块配置
    ///
    /// `use_nvidia_current` 控制使用 nvidia 还是 nvidia-current 模块。
    /// `coolbits` 作为内核模块参数直接写入 modprobe 配置（Wayland 兼容）。
    fn write_modeset_config(
        use_nvidia_current: bool,
        coolbits: Option<u32>,
    ) -> Result<(), FtoolError> {
        let mut content = if use_nvidia_current {
            constants::MODESET_CURRENT_CONTENT.to_string()
        } else {
            constants::MODESET_CONTENT.to_string()
        };

        if let Some(val) = coolbits {
            if use_nvidia_current {
                content.push_str(&format!(
                    "options nvidia-current NVreg_Coolbits={}\n",
                    val
                ));
            } else {
                content.push_str(&format!("options nvidia NVreg_Coolbits={}\n", val));
            }
        }

        create_file(constants::MODESET_PATH, &content, false)
    }

}