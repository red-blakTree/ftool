mod cache;
mod cleanup;
pub mod cli;
mod constants;
mod detector;
mod display;
mod file_io;
mod generator;
mod initramfs;
mod power;
mod services;
mod sleep_config;
mod snapshot;

use crate::core::FtoolError;
use cache::CacheData;
use file_io::{create_file, create_file_bytes};
use log::{error, info, warn};

/// GPU 工作模式枚举
///
/// 用于控制 NVIDIA Optimus 笔记本的显卡切换策略。
/// X11 环境下 Nvidia 模式会写入 PrimaryGPU 配置；
/// Wayland 下由 nvidia-drm modeset 接管显示输出。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuMode {
    /// 仅使用集成显卡，禁用所有 NVIDIA 内核模块
    Integrated,
    /// PRIME 混合模式，按需动态渲染
    Hybrid,
    /// 仅使用 NVIDIA 独立显卡
    Nvidia,
}

impl GpuMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Integrated => "integrated",
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
    /// Nvidia 模式是否启用 ForceCompositionPipeline（修复画面撕裂）
    pub force_comp: bool,
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
        // 进程级单实例锁：并发运行的两个 ftool 实例同时执行切换时，
        // cleanup/快照/写入会交错执行导致系统配置损坏，必须互斥
        let _lock = acquire_instance_lock()?;

        // 先检查系统是否支持 GPU 切换
        if !detector::GpuDetector::can_switch()? {
            return Err(FtoolError::Gpu(
                "此设备不支持 GPU 切换（可能为台式机或没有双显卡）".into(),
            ));
        }

        // 切换前保存配置快照，失败时自动回滚
        let snapshot = snapshot::ConfigSnapshot::save(constants::SNAPSHOT_PATHS)?;

        let result = Self::do_switch(&opts);
        if let Err(ref e) = result {
            warn!("切换失败，正在回滚配置: {}", e);
            // 回滚逐项尽力执行（单项失败不中止其余项），返回失败清单；
            // 清单非空说明回滚不完整、系统处于中间态，必须向用户明确上报。
            // 注意：这里仍返回原始切换错误，回滚细节通过 error! 逐条呈现
            let rollback_failures = snapshot.restore();
            if !rollback_failures.is_empty() {
                error!("配置回滚不完整，以下项目恢复失败（请检查系统状态）:");
                for issue in &rollback_failures {
                    error!("  - {}", issue);
                }
            }
            // 回滚后也重建 initramfs，恢复之前的内核模块/initramfs 状态
            if let Err(rebuild_err) = initramfs::rebuild_initramfs() {
                warn!("回滚后重建 initramfs 失败: {}", rebuild_err);
            }
        }
        result
    }

    /// 执行实际的模式切换（内部使用）
    fn do_switch(opts: &SwitchOptions) -> Result<(), FtoolError> {
        info!("🚀 正在切换到 {} 模式...", opts.mode.as_str());
        cleanup::cleanup()?;

        // 从 Integrated 模式离开时，NVIDIA PCI 设备可能已被 udev 移除。
        // 清理掉移除规则后先做一次显式 rescan，便于后续重新检测和写缓存；
        // rescan 失败不阻断流程，仍可回退到已有缓存。
        if opts.mode != GpuMode::Integrated
            && detector::GpuDetector::query_current_mode() == GpuMode::Integrated
            && let Err(e) = detector::GpuDetector::rescan_pci_bus()
        {
            warn!("PCI rescan 失败（非致命）: {}", e);
        }

        let nv = &opts.nvidia_opts;
        match opts.mode {
            GpuMode::Integrated => Self::switch_integrated()?,
            GpuMode::Hybrid => Self::switch_hybrid(nv.rtd3, nv.use_nvidia_current)?,
            GpuMode::Nvidia => Self::switch_nvidia(nv)?,
        }

        // 写入 PRIME 离散模式标志（参考 system76-power 的实现）
        let prime_mode = match opts.mode {
            GpuMode::Hybrid => "on-demand",
            GpuMode::Nvidia => "on",
            GpuMode::Integrated => "off",
        };
        display::set_prime_discrete(prime_mode)?;

        // 追加挂起电源管理配置（非 Integrated 模式下需要）
        sleep_config::append_sleep_config(opts.mode)?;

        // 重建 initramfs
        initramfs::rebuild_initramfs()?;
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
        // 进程级单实例锁：与切换等变更命令互斥
        let _lock = acquire_instance_lock()?;

        info!("🔄 正在重置 GPU 配置...");
        cleanup::cleanup()?;
        // 禁用 NVIDIA 相关 systemd 服务（与 Integrated 模式策略一致），
        // 否则清理配置后会残留仍处于 enable 状态的 suspend/persistenced 服务，
        // 形成"服务启用但挂起参数已被删除"的不一致中间态
        let svc_issues = Self::configure_gpu_services(false, false, false);
        // cleanup 与服务禁用均已不可逆：此后步骤若失败，系统已处于"半重置"
        // 状态，返回的错误需附注说明，引导用户检查系统状态或手动重建 initramfs
        cache::GpuCache::delete().map_err(with_reset_partial_state_note)?;
        if let Err(e) = initramfs::rebuild_initramfs() {
            return Err(with_reset_partial_state_note(e));
        }
        notify_service_config_issues(&svc_issues);
        info!("✅ 重置成功！请重启计算机以使更改生效。");
        Ok(())
    }

    /// 创建 NVIDIA GPU 缓存（需处于 hybrid 模式）
    pub fn cache_create() -> Result<(), FtoolError> {
        // 进程级单实例锁：与切换等变更命令互斥
        let _lock = acquire_instance_lock()?;
        let mode = detector::GpuDetector::query_current_mode();
        if mode != GpuMode::Hybrid {
            return Err(FtoolError::Input(
                "--cache-create 要求系统当前处于 hybrid 模式".into(),
            ));
        }
        Self::write_nvidia_cache()
    }

    /// 删除 GPU 缓存
    pub fn delete_cache() -> Result<(), FtoolError> {
        // 进程级单实例锁：删除缓存与切换读写同一缓存文件，需互斥
        let _lock = acquire_instance_lock()?;
        cache::GpuCache::delete()
    }

    /// 查询 GPU 缓存内容
    pub fn cache_query() -> Result<String, FtoolError> {
        cache::GpuCache::query()
    }

    /// 运行时电源控制（无需重启，立即生效）
    pub fn power(action: PowerAction) -> Result<(), FtoolError> {
        // 进程级单实例锁：运行时电源控制同样变更系统状态，与切换等互斥
        let _lock = acquire_instance_lock()?;
        match action {
            PowerAction::On => power::runtime_power_on(),
            PowerAction::Off => power::runtime_power_off(),
            PowerAction::Auto => power::auto_power(),
        }
    }

    /// 查询运行时 NVIDIA GPU 电源状态
    pub fn query_power() -> bool {
        power::query_runtime_power()
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
    /// 缓存数据只在 GPU 在线（sysfs 可检测到）时刷新；sysfs 检测不到
    /// NVIDIA（例如 Integrated 模式下已被 udev 移除）时只读校验已有缓存
    /// （版本与格式），不再重写——改写会刷新 mtime，把硬件已变化后的
    /// 陈旧缓存持续"保鲜"，掩盖数据失效。
    fn write_nvidia_cache() -> Result<(), FtoolError> {
        let (pci_bus, device_ids) = match detector::GpuDetector::get_nvidia_raw_pci_id() {
            Ok(raw) => {
                // 将原始 DDDD:BB:DD.F 格式标准化为 "PCI:BB:DD:F" 写入缓存
                let without_domain = raw.split_once(':').map(|(_, r)| r).unwrap_or(&raw);
                let parts: Vec<&str> = without_domain.split(':').collect();
                if parts.len() != 2 {
                    return Err(FtoolError::Gpu(format!("PCI 设备 ID 格式异常: {}", raw)));
                }
                let dev_func: Vec<&str> = parts[1].split('.').collect();
                if dev_func.len() != 2 {
                    return Err(FtoolError::Gpu(format!("PCI 设备 ID 格式异常: {}", raw)));
                }
                let bus = u32::from_str_radix(parts[0], 16)
                    .map_err(|_| FtoolError::Gpu(format!("PCI Bus 解析失败: {}", raw)))?;
                let dev = u32::from_str_radix(dev_func[0], 16)
                    .map_err(|_| FtoolError::Gpu(format!("PCI Dev 解析失败: {}", raw)))?;
                let func = u32::from_str_radix(dev_func[1], 16)
                    .map_err(|_| FtoolError::Gpu(format!("PCI Func 解析失败: {}", raw)))?;
                let bus_str = format!("PCI:{}:{}:{}", bus, dev, func);
                // 写入前做与读端（GpuCache::read）一致的格式/范围校验：hex→u32
                // 解析可能产出越界值（如 bus>255 或带前导零），避免把非法地址
                // 写入缓存；失败时带原始 sysfs 值报错便于排障
                if !cache::GpuCache::validate_pci_bus(&bus_str) {
                    return Err(FtoolError::Gpu(format!(
                        "PCI 设备 ID 解析结果超出有效范围: {}",
                        raw
                    )));
                }

                // GPU 在线时同时收集所有 NVIDIA 设备 ID（用于 PCIe 断电后恢复）。
                // rescan 后设备枚举是异步的，首次收集失败时短暂重试一次，
                // 避免竞态下静默把空设备列表写入缓存
                let mut collected = detector::GpuDetector::get_all_nvidia_device_ids();
                if collected.is_err() {
                    std::thread::sleep(std::time::Duration::from_millis(500));
                    collected = detector::GpuDetector::get_all_nvidia_device_ids();
                }
                let ids: Vec<cache::NvidiaDeviceId> = match collected {
                    Ok(ids) => ids,
                    Err(e) => {
                        warn!("收集 NVIDIA 设备 ID 失败，缓存将不含设备 ID; error={}", e);
                        Vec::new()
                    }
                };
                (bus_str, ids)
            }
            Err(_) => {
                // Fallback: sysfs 检测不到 NVIDIA（如 Integrated 模式下被 udev 移除）。
                // 只读校验已有缓存（版本与格式）后直接返回——校验通过说明缓存
                // 仍可用（作为后续模式切换的后备数据源），但不在 GPU 离线时
                // 改写缓存，避免陈旧缓存被 mtime 持续"保鲜"
                let data = match cache::GpuCache::read() {
                    Ok(data) => data,
                    // 从未写过缓存：无后备数据可用，报错合理
                    Err(_) if !std::path::Path::new(constants::CACHE_FILE_PATH).exists() => {
                        return Err(FtoolError::Gpu(
                            "sysfs 未检测到 NVIDIA 显卡且无缓存数据，无法保存缓存。".into(),
                        ));
                    }
                    // 缓存存在但损坏/版本不符：透出具体错误，便于区分
                    // "从未缓存"与"缓存不可用"两种情形
                    Err(e) => {
                        return Err(FtoolError::Gpu(format!(
                            "sysfs 未检测到 NVIDIA 显卡，且现有缓存不可用: {}",
                            e
                        )));
                    }
                };
                info!(
                    "sysfs 未检测到 NVIDIA，已有缓存校验通过（不重写，避免陈旧缓存被保鲜）; bus={}",
                    data.nvidia_gpu_pci_bus
                );
                return Ok(());
            }
        };
        cache::GpuCache::write(&CacheData::new(pci_bus, device_ids))
    }

    /// 统一配置 NVIDIA GPU 相关 systemd 服务，消除 switch_* 中的重复代码
    ///
    /// 各模式对服务的需求：
    /// - Integrated:  全部禁用（persistenced=false, fallback=false, suspend=false）
    /// - Hybrid:      persistenced + suspend（persistenced=true,  fallback=false, suspend=true）
    /// - Nvidia:      全部启用（persistenced=true,  fallback=true,  suspend=true）
    ///
    /// 返回"未能按目标状态配置"的服务说明列表（空 = 全部配置成功），流程不中止。
    /// 服务未安装（如 Fedora 无 nvidia-fallback.service）或 static/masked 等
    /// 不可变更状态不算硬错误，仅记为"跳过"；只有真实 enable/disable 失败才
    /// 记为错误。清单由各 switch_*/reset 在成功路径上以用户可见方式提示。
    fn configure_gpu_services(persistenced: bool, fallback: bool, suspend: bool) -> Vec<String> {
        let mut problems: Vec<String> = Vec::new();
        for (service, enable) in [
            ("nvidia-persistenced.service", persistenced),
            ("nvidia-fallback.service", fallback),
        ] {
            match services::ensure_service_state(service, enable) {
                Ok(services::ServiceConfigOutcome::Configured) => {}
                Ok(services::ServiceConfigOutcome::Skipped) => {
                    problems.push(services::service_config_issue_message(
                        service, enable, None,
                    ));
                }
                Err(e) => problems.push(services::service_config_issue_message(
                    service,
                    enable,
                    Some(&e.to_string()),
                )),
            }
        }
        problems.extend(services::configure_nvidia_suspend_services(suspend));
        problems
    }

    /// Integrated 模式：完全禁用 NVIDIA 驱动，仅使用集成显卡
    fn switch_integrated() -> Result<(), FtoolError> {
        // 保存 NVIDIA GPU PCI 地址缓存，供后续从 Integrated 模式切换时使用
        // （核显模式的 udev 规则会物理移除 NVIDIA PCI 设备，缓存是唯一后备数据源）
        if let Err(e) = Self::write_nvidia_cache() {
            warn!("保存 NVIDIA GPU 缓存失败，继续执行; error={}", e);
        }

        let svc_issues = Self::configure_gpu_services(false, false, false);

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
            match std::fs::remove_file(constants::MODESET_PATH) {
                Ok(()) => {}
                // 检查后被并发删除等竞态与"文件本就不存在"等价，忽略
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => warn!(
                    "删除 modeset 残留文件失败 {}: {}",
                    constants::MODESET_PATH,
                    e
                ),
            }
        }

        notify_service_config_issues(&svc_issues);
        Ok(())
    }

    /// Hybrid 模式：PRIME 按需渲染，支持 RTD3 动态电源管理
    fn switch_hybrid(rtd3: Option<u32>, use_nvidia_current: bool) -> Result<(), FtoolError> {
        let svc_issues = Self::configure_gpu_services(true, false, true);

        // 写入空 modprobe 配置（允许所有驱动正常加载）
        create_file_bytes(constants::MODPROBE_GPU_PATH, constants::MODPROBE_EMPTY)?;

        // 写入 modeset 配置（含 RTD3 电源管理参数）
        let modeset_content =
            generator::ConfigGenerator::generate_modeset_content(rtd3, use_nvidia_current);
        create_file(constants::MODESET_PATH, &modeset_content, false)?;

        // 写入 Hybrid 专用 udev 电源管理规则（移除 Audio/USB/UCSI 设备以节省电量）
        create_file(constants::UDEV_PM_PATH, constants::UDEV_PM_CONTENT, false)?;

        Self::write_nvidia_cache()?;

        notify_service_config_issues(&svc_issues);
        Ok(())
    }

    /// Nvidia 模式：仅使用 NVIDIA 独立显卡输出画面
    ///
    /// 写入 X11 PrimaryGPU 配置使 NVIDIA 成为主显示器（参考 system76-power），
    /// 同时写入 nvidia-drm modeset=1 确保 Wayland 下的 DRM 直通输出，
    /// 并补充 ForceCompositionPipeline/Coolbits 等 Xorg 选项与 DM 桥接脚本。
    fn switch_nvidia(opts: &NvidiaOptions) -> Result<(), FtoolError> {
        let svc_issues = Self::configure_gpu_services(true, true, true);

        // 写入空 modprobe 配置
        create_file_bytes(constants::MODPROBE_GPU_PATH, constants::MODPROBE_EMPTY)?;

        // 写入 modeset 配置（不含 Coolbits；Coolbits 写入 Xorg 额外配置）
        Self::write_modeset_config(opts.use_nvidia_current)?;

        // 写入 X11 PrimaryGPU 配置（参考 system76-power 的 discrete 模式）
        display::write_xorg_nvidia_config()?;

        // 写入 NVIDIA 额外 Xorg 配置（ForceCompositionPipeline / Coolbits）
        display::write_xorg_nvidia_extra_config(opts)?;
        // Display Manager 适配：SDDM / LightDM 的 xrandr 桥接脚本
        display::write_dm_scripts()?;

        // 写入 NVIDIA 环境变量配置，确保应用使用 NVIDIA 渲染
        display::write_nvidia_env_config()?;

        notify_service_config_issues(&svc_issues);
        Ok(())
    }

    /// 写入 NVIDIA modeset 内核模块配置
    ///
    /// `use_nvidia_current` 控制使用 nvidia 还是 nvidia-current 模块。
    fn write_modeset_config(use_nvidia_current: bool) -> Result<(), FtoolError> {
        let content = if use_nvidia_current {
            constants::MODESET_CURRENT_CONTENT
        } else {
            constants::MODESET_CONTENT
        };
        create_file(constants::MODESET_PATH, content, false)
    }
}

/// 变更命令的进程级单实例锁文件路径（/run 优先；变更命令均需 root 运行）
const INSTANCE_LOCK_PATH: &str = "/run/ftool.lock";

/// 获取进程级单实例建议锁
///
/// 并发运行两个 ftool 实例执行变更命令会交错 cleanup/快照/写入，导致
/// 系统配置损坏。返回：
/// - `Ok(Some(file))`：持锁成功，file 需保持存活到命令结束（drop 关闭
///   fd 时自动释放 flock）；
/// - `Ok(None)`：建锁本身失败（如 /run 不存在/无权限），warn 后继续，
///   尽力而为，避免锁机制问题阻断功能；
/// - `Err`：锁已被其它实例占用（LOCK_NB 返回 EWOULDBLOCK），调用方应中止命令。
#[cfg(unix)]
fn acquire_instance_lock() -> Result<Option<std::fs::File>, FtoolError> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;

    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        // 锁文件内容无意义（只使用 fd 上的 flock），打开时不截断不清空
        .truncate(false)
        .create(true)
        .mode(0o600)
        .open(INSTANCE_LOCK_PATH)
    {
        Ok(file) => file,
        Err(e) => {
            warn!("无法打开单实例锁文件，继续执行（尽力而为）: {}", e);
            return Ok(None);
        }
    };

    let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        // 锁已被占用是唯一需要中止命令的情形（明确提示用户稍后再试）
        if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
            return Err(FtoolError::Gpu(
                "另一个 ftool 实例正在运行，请稍后再试".into(),
            ));
        }
        warn!("单实例加锁失败，继续执行（尽力而为）: {}", err);
        return Ok(None);
    }
    Ok(Some(file))
}

#[cfg(not(unix))]
fn acquire_instance_lock() -> Result<Option<std::fs::File>, FtoolError> {
    Ok(None)
}

/// 打印"服务未能按模式配置"的用户可见警告
///
/// 在成功路径上由各 switch_*/reset 调用：配置失败不阻断切换，但用户
/// 必须知道哪些服务实际未按目标模式配置，故用 println!（面向用户）输出
fn notify_service_config_issues(issues: &[String]) {
    if issues.is_empty() {
        return;
    }
    println!("⚠️ 注意：以下 NVIDIA 服务未能按模式配置（重启后相关功能可能不完整）:");
    for issue in issues {
        println!("  - {}", issue);
    }
}

/// 为 reset 后半段（不可逆清理完成后）的失败错误附加"半重置状态"说明
fn with_reset_partial_state_note(e: FtoolError) -> FtoolError {
    FtoolError::Gpu(format!(
        "{}（注意：配置与服务已清理，仅最后步骤失败；请检查系统状态后重试或手动重建 initramfs）",
        e
    ))
}
