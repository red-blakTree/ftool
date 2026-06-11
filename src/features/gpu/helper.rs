use crate::core::FtoolError;
use crate::core::runner::CommandRunner;
use crate::features::gpu::constants::*;
use crate::features::gpu::detector::{GpuDetector, SleepMode};
use log::{debug, info, warn};
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// 原子方式写入文件：先写临时文件再 rename，确保写入原子性
///
/// 如果 `executable` 为 true，会赋予执行权限（仅限文本文件）。
/// `content` 接受字节切片，统一处理文本和二进制内容。
fn write_file_atomic(path: &str, content: &[u8], executable: bool) -> Result<(), FtoolError> {
    if let Some(parent) = Path::new(path).parent() {
        fs::create_dir_all(parent)
            .map_err(|e| FtoolError::Gpu(format!("创建目录失败 {:?}: {}", parent, e)))?;
    }

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let tmp_path = format!("{}.tmp.{}-{}", path, std::process::id(), ts);
    fs::write(&tmp_path, content)
        .map_err(|e| FtoolError::Gpu(format!("写入临时文件失败 {}: {}", tmp_path, e)))?;
    debug!("临时文件已写入; path={}", tmp_path);

    if executable {
        let mut perms = fs::metadata(&tmp_path)
            .map_err(|e| FtoolError::Gpu(format!("获取文件元数据失败 {}: {}", tmp_path, e)))?
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&tmp_path, perms)
            .map_err(|e| FtoolError::Gpu(format!("设置权限失败 {}: {}", tmp_path, e)))?;
        debug!("已赋予执行权限; path={}", tmp_path);
    }

    fs::rename(&tmp_path, path)
        .map_err(|e| FtoolError::Gpu(format!("重命名文件到 {} 失败: {}", path, e)))?;
    debug!("配置文件已生效; path={}", path);
    Ok(())
}

/// 原子方式写入文本文件（可选赋予可执行权限）
pub fn create_file(path: &str, content: &str, executable: bool) -> Result<(), FtoolError> {
    write_file_atomic(path, content.as_bytes(), executable)
}

/// 原子方式写入二进制内容文件
pub fn create_file_bytes(path: &str, content: &[u8]) -> Result<(), FtoolError> {
    write_file_atomic(path, content, false)
}

/// 清理所有由 ftool 生成的系统配置文件
pub fn cleanup() -> Result<(), FtoolError> {
    info!("🧹 清理旧的配置文件...");
    let to_remove: &[&str] = &[
        // ftool 自身生成的配置路径（仅 /etc/ 下的文件）
        MODPROBE_GPU_PATH,
        MODESET_PATH,
        UDEV_INTEGRATED_PATH,
        UDEV_PM_PATH,
        PRIME_DISCRETE_PATH,
        // NVIDIA 独显模式环境变量配置
        NV_ENV_PATH,
        // 兼容旧版配置的路径（用于清理升级前的遗留文件）
        "/etc/X11/xorg.conf",
        "/etc/X11/xorg.conf.d/11-nvidia-discrete.conf",
        "/usr/share/X11/xorg.conf.d/11-nvidia-discrete.conf",
        "/etc/X11/xorg.conf.d/10-nvidia.conf",
        "/etc/X11/xorg.conf.d/90-nvidia.conf",
        "/etc/lightdm/nvidia.sh",
        "/etc/lightdm/lightdm.conf.d/20-nvidia.conf",
        "/usr/share/sddm/scripts/Xsetup",
        "/etc/gdm/Init/Default",
        "/etc/gdm/custom.conf",
        LEGACY_BLACKLIST_PATH,
        LEGACY_MODESET_PATH,
        // 注意：/lib/udev/rules.d/ 下的文件由包管理器管理，不在此处删除
    ];

    for path in to_remove {
        debug!("尝试删除文件; path={}", path);
        if let Err(e) = fs::remove_file(path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            warn!("无法删除文件; path={}, error={}", path, e);
        }
    }

    Ok(())
}

/// 重建 initramfs（支持 dracut 和 update-initramfs，以及 OSTree 系统）
pub fn rebuild_initramfs() -> Result<(), FtoolError> {
    info!("⚙️ 正在重建 initramfs...");

    // OSTree 系系统（如 Fedora Silverblue）
    if Path::new("/ostree").exists() || Path::new("/sysroot/ostree").exists() {
        info!("检测到 OSTree 系统，使用 rpm-ostree...");
        let status =
            CommandRunner::run_status("rpm-ostree", ["initramfs", "--enable", "--arg=--force"])?;
        return CommandRunner::ensure_success(status);
    }

    // Debian/Ubuntu 使用 update-initramfs
    if Path::new("/usr/sbin/update-initramfs").exists()
        || Path::new("/sbin/update-initramfs").exists()
    {
        info!("检测到 update-initramfs，使用 Debian 方式...");
        let status = CommandRunner::run_status("update-initramfs", ["-u"])?;
        return CommandRunner::ensure_success(status);
    }

    // 其他发行版使用 dracut
    let mut cmd: Vec<&str> = vec!["dracut", "--force", "--regenerate-all"];

    // 使用 systemd-inhibit 防止关机中断重建过程
    if Path::new("/usr/bin/systemd-inhibit").exists() {
        debug!("检测到 systemd-inhibit，将使用它防止关机中断重建过程");
        cmd = vec![
            "systemd-inhibit",
            "--who=ftool",
            "--why",
            "Rebuilding initramfs",
            "--",
        ]
        .into_iter()
        .chain(cmd)
        .collect();
    }

    debug!("执行命令; cmd={:?}", cmd);
    let status = CommandRunner::run_status(cmd[0], &cmd[1..])?;
    CommandRunner::ensure_success(status)
}

/// 获取 X11 配置文件路径，自动检测可用目录
///
/// 参考 system76-power 的 `get_xorg_conf_path` 实现。
pub fn get_xorg_conf_path() -> &'static str {
    if Path::new("/etc/X11/xorg.conf.d").exists() {
        XORG_CONF_NVIDIA_PATH
    } else {
        XORG_CONF_NVIDIA_FALLBACK_PATH
    }
}

/// 写入 NVIDIA 离散模式 X11 PrimaryGPU 配置
///
/// 在 Nvidia 模式下设置 PrimaryGPU "Yes"，使 NVIDIA 成为 X11 主显示输出。
/// 参考 system76-power 的 discrete 模式 Xorg 配置逻辑。
pub fn write_xorg_nvidia_config() -> Result<(), FtoolError> {
    let path = get_xorg_conf_path();
    info!("写入 X11 配置; path={}", path);
    create_file(path, XORG_CONF_NVIDIA_CONTENT, false)
}

/// 写入 NVIDIA 独显模式环境变量配置
///
/// 在 Nvidia 模式下写入 /etc/environment.d/ftool-nvidia.conf，
/// 设置 __NV_PRIME_RENDER_OFFLOAD=1 和 __GLX_VENDOR_LIBRARY_NAME=nvidia，
/// 确保应用默认使用 NVIDIA 渲染。
pub fn write_nvidia_env_config() -> Result<(), FtoolError> {
    info!("写入 NVIDIA 环境变量配置; path={}", NV_ENV_PATH);
    create_file(NV_ENV_PATH, NV_ENV_CONTENT, false)
}

/// 启用或禁用 systemd 服务
///
/// 当服务操作失败时返回 `FtoolError::Process`。
/// 调用方可根据场景决定是否忽略（如 `configure_nvidia_suspend_services`）。
pub fn toggle_service(name: &str, enable: bool) -> Result<(), FtoolError> {
    let action = if enable { "enable" } else { "disable" };
    let status = CommandRunner::run_status("systemctl", [action, name])?;
    if status.success() {
        info!("已成功变更服务状态; action={}, service={}", action, name);
        Ok(())
    } else {
        Err(FtoolError::Process(format!(
            "{} {} 失败 (exit: {:?})",
            action,
            name,
            status.code()
        )))
    }
}

/// 管理 NVIDIA 挂起/休眠/恢复服务的启用/禁用
///
/// 为 NVIDIA_SUSPEND_SERVICES 列表中每个服务执行 enable/disable。
/// 服务不存在或操作失败时仅记录 warn 级别日志，不会阻断流程。
pub fn configure_nvidia_suspend_services(enable: bool) -> Result<(), FtoolError> {
    for service in NVIDIA_SUSPEND_SERVICES {
        if let Err(e) = toggle_service(service, enable) {
            warn!(
                "NVIDIA 挂起服务操作失败，将忽略此错误; service={}, error={}",
                service, e
            );
        }
    }
    Ok(())
}

/// 写入 PRIME 离散模式标志文件
pub fn set_prime_discrete(mode: &str) -> Result<(), FtoolError> {
    info!("设置 {} 为 {}", PRIME_DISCRETE_PATH, mode.trim());
    create_file(PRIME_DISCRETE_PATH, mode, false)
}

/// 根据系统挂起模式，追加对应的 NVIDIA 电源管理配置
///
/// Integrated 模式下此函数为 no-op（不写入任何睡眠配置）。
/// 其他模式通过"读现有内容 → 合并 → 原子写"的方式写入，
/// 确保写入操作整体原子性，避免在追加写入时若崩溃导致文件损坏。
pub fn append_sleep_config(mode: super::GpuMode) -> Result<(), FtoolError> {
    if mode == super::GpuMode::Integrated {
        return Ok(());
    }

    let sleep_mode = GpuDetector::detect_sleep_mode();
    let sleep_content = match sleep_mode {
        SleepMode::S0ix => MODPROBE_S0IX,
        SleepMode::S3 => MODPROBE_S3,
    };

    let path = MODPROBE_GPU_PATH;

    // 读取现有内容（文件可能不存在）
    let existing = fs::read_to_string(path).unwrap_or_default();

    // 用关键行做精确检查，避免因空白/顺序差异误判
    let key_line = match sleep_mode {
        SleepMode::S0ix => "NVreg_EnableS0ixPowerManagement=1",
        SleepMode::S3 => "NVreg_PreserveVideoMemoryAllocations=1",
    };
    let sleep_text = String::from_utf8_lossy(sleep_content);
    if existing.contains(key_line) {
        debug!("挂起配置已存在，跳过写入");
    } else {
        // 合并内容并原子写入
        let merged = if existing.trim().is_empty()
            || existing.trim_end()
                == "# Automatically generated by ftool"
        {
            // 文件为空或只有头部注释 → 直接写入挂起配置
            format!("{}\n", sleep_text.trim())
        } else {
            // 追加到现有内容后
            format!("{}\n{}", existing.trim_end(), sleep_text.trim())
        };
        create_file(path, &merged, false)?;
    }

    // 启用 NVIDIA 挂起服务
    configure_nvidia_suspend_services(true)?;

    Ok(())
}

// ========== 运行时电源控制 ==========

/// 运行时开启 NVIDIA GPU（无需重启）
pub fn runtime_power_on() -> Result<(), FtoolError> {
    info!("⚡ 运行时开启 NVIDIA GPU...");

    // 重新扫描 PCI 总线
    fs::write("/sys/bus/pci/rescan", "1")
        .map_err(|e| FtoolError::Gpu(format!("PCI rescan 失败: {}", e)))?;

    // 同步等待后设置电源管理（参考 system76-power 的做法）
    if let Ok(pci_id) = GpuDetector::get_nvidia_raw_pci_id() {
        let mode = GpuDetector::query_current_mode();
        apply_power_control(&pci_id, mode)?;
    }

    Ok(())
}

/// 检查 NVIDIA GPU 上是否有运行中的进程
///
/// 使用带超时的 run_with_timeout（5 秒），防止 nvidia-smi
/// 在驱动异常时永久阻塞。超时或执行失败均视为"无进程"。
/// 当 nvidia-smi 不可用时输出 warn 日志并跳过检查。
const NVIDIA_SMI_TIMEOUT_SECS: u64 = 5;

fn has_nvidia_processes() -> bool {
    // 检查 nvidia-smi 是否可用
    let smi_path = Path::new("/usr/bin/nvidia-smi");
    if !smi_path.exists() {
        warn!("nvidia-smi 未安装，无法检测 GPU 进程状态，跳过安全检查");
        return false;
    }
    // 检查计算类进程（CUDA、机器学习等）
    if let Ok(output) = CommandRunner::run_with_timeout(
        "nvidia-smi",
        [
            "--query-compute-apps=pid,process_name",
            "--format=csv,noheader",
        ],
        NVIDIA_SMI_TIMEOUT_SECS,
    ) {
        let stdout = String::from_utf8_lossy(&output.stdout);
        if stdout.lines().any(|line| !line.trim().is_empty()) {
            return true;
        }
    }
    // 同时检查图形渲染进程（OpenGL/Vulkan 等）
    // 在 Hybrid/Nvidia 模式下可能有图形进程使用 GPU 但不被 compute-apps 列出
    if let Ok(output) = CommandRunner::run_with_timeout(
        "nvidia-smi",
        [
            "--query-graphics-apps=pid,process_name",
            "--format=csv,noheader",
        ],
        NVIDIA_SMI_TIMEOUT_SECS,
    ) {
        let stdout = String::from_utf8_lossy(&output.stdout);
        stdout.lines().any(|line| !line.trim().is_empty())
    } else {
        false
    }
}

/// 运行时关闭 NVIDIA GPU（无需重启）
pub fn runtime_power_off() -> Result<(), FtoolError> {
    info!("💤 运行时关闭 NVIDIA GPU...");

    // 安全检查：确认无进程正在使用 NVIDIA GPU
    if has_nvidia_processes() {
        return Err(FtoolError::Gpu(
            "NVIDIA GPU 上存在运行中的进程，请先终止它们（如 nvidia-smi 查询结果所示）".into(),
        ));
    }

    let pci_id = GpuDetector::get_nvidia_raw_pci_id()?;

    // 查找同 slot 的所有 function
    let pci_path = Path::new("/sys/bus/pci/devices");
    let slot = pci_id.split('.').next().unwrap_or("");
    let entries = fs::read_dir(pci_path)
        .map_err(|e| FtoolError::Gpu(format!("读取 PCI 设备目录失败: {}", e)))?;

    let mut functions: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.split('.').next().unwrap_or("") == slot {
            // 检查是否为 NVIDIA 设备
            let vendor_path = pci_path.join(name_str.as_ref()).join("vendor");
            if let Ok(vendor) = fs::read_to_string(&vendor_path)
                && vendor.trim() == "0x10de"
            {
                functions.push(name_str.into_owned());
            }
        }
    }

    // 步骤1：解绑所有 NVIDIA 设备的驱动（收集错误，不提前终止）
    let mut unbind_errors: Vec<String> = Vec::new();
    for func_id in &functions {
        let func_path = pci_path.join(func_id);
        let driver_link = func_path.join("driver");

        if let Ok(driver_path) = driver_link.read_link() {
            let driver_name = driver_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");
            let unbind_path = format!("/sys/bus/pci/drivers/{}/unbind", driver_name);
            debug!("解绑驱动; driver={}, device={}", driver_name, func_id);
            if let Err(e) = fs::write(&unbind_path, func_id) {
                warn!("解绑 function 失败，继续处理其他 function; func={}, error={}", func_id, e);
                unbind_errors.push(format!("{}: {}", func_id, e));
            }
        }
    }

    // 步骤2：从 PCI 总线移除设备（收集错误，不提前终止）
    let mut remove_errors: Vec<String> = Vec::new();
    for func_id in &functions {
        let remove_path = format!("/sys/bus/pci/devices/{}/remove", func_id);
        debug!("移除 PCI 设备; device={}", func_id);
        if let Err(e) = fs::write(&remove_path, "1") {
            warn!("移除 function 失败，继续处理其他 function; func={}, error={}", func_id, e);
            remove_errors.push(format!("{}: {}", func_id, e));
        }
    }

    // 汇总错误：只要有任何操作失败就返回错误
    if !unbind_errors.is_empty() || !remove_errors.is_empty() {
        let mut msg = String::from("关闭 NVIDIA GPU 电源时部分操作失败");
        if !unbind_errors.is_empty() {
            msg.push_str(&format!("; 解绑失败: {}", unbind_errors.join(", ")));
        }
        if !remove_errors.is_empty() {
            msg.push_str(&format!("; 移除失败: {}", remove_errors.join(", ")));
        }
        return Err(FtoolError::Gpu(msg));
    }

    Ok(())
}

/// 查询 NVIDIA GPU 电源状态（是否在线）
pub fn query_runtime_power() -> bool {
    GpuDetector::is_nvidia_online()
}

/// 轮询等待 NVIDIA 驱动绑定完成，然后设置 PCI 运行时电源管理
///
/// 参考 system76-power：NVIDIA 驱动初始化后过早修改电源管理属性
/// 可能导致系统锁死。采用轮询 + 超时机制替代固定 sleep：
/// - 先等待驱动绑定（最多 10 秒）
/// - 驱动绑定后再等待 2 秒让 NVIDIA 完成内部初始化
/// - 同步阻塞，避免后台线程因进程退出而夭折
fn apply_power_control(pci_id: &str, mode: super::GpuMode) -> Result<(), FtoolError> {
    let driver_link = format!("/sys/bus/pci/devices/{}/driver", pci_id);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    info!("等待 NVIDIA 驱动绑定; device={}", pci_id);
    
    let mut bound = false;
    while std::time::Instant::now() < deadline {
        if let Ok(link) = std::fs::read_link(&driver_link)
            && let Some(name) = link.file_name().and_then(|n| n.to_str())
        {
            debug!("NVIDIA 驱动已绑定; driver={}", name);
            bound = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
    
    if !bound {
        warn!("NVIDIA 驱动未在 10 秒内绑定，仍尝试设置电源管理");
    } else {
        info!("驱动已绑定，等待内部初始化完成（2 秒）...");
        std::thread::sleep(std::time::Duration::from_secs(2));
    }

    let pm = if mode == super::GpuMode::Nvidia {
        "on\n"
    } else {
        "auto\n"
    };
    info!("设置电源管理为 {}", pm.trim());

    let control = format!("/sys/bus/pci/devices/{}/power/control", pci_id);
    let mut file = fs::OpenOptions::new()
        .create(false)
        .truncate(false)
        .write(true)
        .open(&control)
        .map_err(|e| FtoolError::Gpu(format!("打开 {} 失败: {}", control, e)))?;

    file.write_all(pm.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|e| FtoolError::Gpu(format!("设置电源管理失败: {}", e)))?;

    Ok(())
}

/// 自动设置 GPU 电源状态（基于当前模式和 runtimepm 支持判断）
///
/// 参考 system76-power 的 `auto_power` 逻辑：
/// - 非 Integrated 模式 → 开启电源
/// - Integrated 模式但 GPU 支持 runtime PM → 开启电源（允许运行时挂起省电）
/// - Integrated 模式且 GPU 不支持 runtime PM → 关闭电源
pub fn auto_power() -> Result<(), FtoolError> {
    let mode = GpuDetector::query_current_mode();
    let should_power_on = if mode == super::GpuMode::Integrated {
        // Integrated 模式下根据 runtimepm 支持决定是否保持 GPU 在线
        match GpuDetector::gpu_supports_runtimepm() {
            Ok(runtimepm) => runtimepm,
            Err(err) => {
                log::warn!("无法判断 runtimepm 支持，默认关闭 GPU 电源: {}", err);
                false
            }
        }
    } else {
        true
    };

    if should_power_on {
        runtime_power_on()
    } else if GpuDetector::is_nvidia_online() {
        runtime_power_off()
    } else {
        Ok(())
    }
}
