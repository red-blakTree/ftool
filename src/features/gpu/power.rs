//! NVIDIA GPU 运行时电源控制（无需重启，通过 sysfs 直接操作 PCI 设备）。

use super::GpuMode;
use crate::core::FtoolError;
use crate::features::gpu::constants::UDEV_INTEGRATED_PATH;
use crate::features::gpu::detector::GpuDetector;
use log::{debug, info, warn};
use std::fs;
use std::io::Write;
use std::path::Path;

/// NVIDIA PCI vendor ID（sysfs vendor 文件中的文本形式）
const NVIDIA_VENDOR: &str = "0x10de";

/// 运行时开启 NVIDIA GPU（无需重启）
pub(super) fn runtime_power_on() -> Result<(), FtoolError> {
    info!("⚡ 运行时开启 NVIDIA GPU...");

    // 重新扫描 PCI 总线，让被移除/未枚举的 NVIDIA 设备重新出现
    GpuDetector::rescan_pci_bus()?;

    // rescan 由内核异步完成枚举，轮询等待 NVIDIA GPU 重新出现在 sysfs 中，
    // 不再静默吞掉检测失败（否则会向用户报告成功而 GPU 实际未开启）
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let pci_id = loop {
        if let Ok(id) = GpuDetector::get_nvidia_raw_pci_id() {
            break id;
        }
        if std::time::Instant::now() >= deadline {
            let hint = if Path::new(UDEV_INTEGRATED_PATH).exists() {
                "；注意当前存在 integrated 模式的 udev 移除规则，设备出现后会被再次移除，无法保持开启"
            } else {
                ""
            };
            return Err(FtoolError::Gpu(format!(
                "PCI rescan 后 10 秒内未检测到 NVIDIA GPU{}",
                hint
            )));
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    };

    // 等驱动绑定完成后设置电源管理（参考 system76-power 的做法）
    let mode = GpuDetector::query_current_mode();
    apply_power_control(&pci_id, mode)?;
    Ok(())
}

/// 通过扫描 /proc/<pid>/fd 检测正在使用 NVIDIA GPU 的进程
///
/// NVIDIA 用户态驱动（CUDA/GLX/Vulkan）通过打开 /dev/nvidia* 字符设备
/// 与内核驱动交互；nvidia-drm 显示路径则打开 /dev/dri/cardN|renderDN。
/// 直接扫描进程 fd 即可发现 GPU 使用者，不依赖 nvidia-smi
/// （该工具并非所有安装场景都存在）。
///
/// 以 root 运行时才能读取其他进程的 fd；个别进程读取失败（权限不足或
/// 恰好退出）视为无 GPU 使用，予以跳过。nvidia-persistenced 服务为保持
/// GPU 初始化而常驻持有 /dev/nvidiactl，不计入"运行中的进程"。
fn has_nvidia_processes() -> Result<bool, FtoolError> {
    let proc_dir = Path::new("/proc");
    let entries =
        fs::read_dir(proc_dir).map_err(|e| FtoolError::Gpu(format!("无法读取 /proc: {}", e)))?;

    for entry in entries.flatten() {
        let pid = entry.file_name();
        let pid_str = pid.to_string_lossy();
        if !pid_str.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }

        // nvidia-persistenced 常驻持有 /dev/nvidiactl，不代表运行中的工作负载
        let comm = fs::read_to_string(proc_dir.join(&pid).join("comm")).unwrap_or_default();
        if comm.trim() == "nvidia-persistenced" {
            continue;
        }

        let fd_dir = entry.path().join("fd");
        let Ok(fds) = fs::read_dir(&fd_dir) else {
            // 进程已退出或权限不足（root 下通常可读），跳过该进程
            continue;
        };
        for fd in fds.flatten() {
            let Ok(target) = fs::read_link(fd.path()) else {
                continue;
            };
            if fd_target_is_nvidia(&target.to_string_lossy()) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// 判断文件描述符目标是否为 NVIDIA GPU 设备节点
///
/// - `/dev/nvidia*`（nvidiactl / nvidia0 / nvidia-uvm / nvidia-modeset 等）直接命中；
/// - `/dev/dri/cardN|renderDN` 需经 sysfs 确认其 PCI vendor 为 NVIDIA
///   （同一系统中的 DRM 节点也可能来自 Intel/AMD 核显）。
fn fd_target_is_nvidia(target: &str) -> bool {
    if target.starts_with("/dev/nvidia") {
        return true;
    }
    let Some(name) = target.strip_prefix("/dev/dri/") else {
        return false;
    };
    if !(name.starts_with("card") || name.starts_with("renderD")) {
        return false;
    }
    let vendor_path = Path::new("/sys/class/drm").join(name).join("device/vendor");
    fs::read_to_string(vendor_path)
        .map(|v| v.trim() == NVIDIA_VENDOR)
        .unwrap_or(false)
}

/// 收集与 `pci_id` 同一 slot 的所有 NVIDIA function 设备名
///
/// 返回按功能号降序排列的列表：先处理子设备（高功能号）再处理父设备
/// （功能号 0），保证解绑/移除顺序正确。仅读取失败（目录不可读）报错；
/// 单个设备的 vendor 读取失败视为非 NVIDIA 跳过。
fn nvidia_functions_desc(pci_id: &str) -> Result<Vec<String>, FtoolError> {
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
                && vendor.trim() == NVIDIA_VENDOR
            {
                functions.push(name_str.into_owned());
            }
        }
    }

    // 按功能号降序排列：先解绑子设备（高功能号）再解绑父设备（功能号 0）
    functions.sort_by(|a, b| {
        let fa = a
            .split('.')
            .next_back()
            .and_then(|f| f.parse::<u32>().ok())
            .unwrap_or(0);
        let fb = b
            .split('.')
            .next_back()
            .and_then(|f| f.parse::<u32>().ok())
            .unwrap_or(0);
        fb.cmp(&fa)
    });
    Ok(functions)
}

/// 步骤 1：按功能号降序解绑所有 NVIDIA 设备的驱动
///
/// 记录已解绑设备与对应驱动，任一解绑失败时尝试重新绑定已解绑设备并
/// 额外 PCI rescan 恢复，随后返回错误（已尝试恢复）。
fn unbind_functions(functions: &[String]) -> Result<(), FtoolError> {
    let pci_path = Path::new("/sys/bus/pci/devices");
    let mut unbound: Vec<(String, String)> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    for func_id in functions {
        let func_path = pci_path.join(func_id);
        if let Ok(driver_link) = fs::read_link(func_path.join("driver")) {
            let driver_name = driver_link
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");
            let unbind_path = format!("/sys/bus/pci/drivers/{}/unbind", driver_name);
            debug!("解绑驱动; driver={}, device={}", driver_name, func_id);
            if let Err(e) = fs::write(&unbind_path, func_id) {
                warn!(
                    "解绑 function 失败，继续处理其他 function; func={}, error={}",
                    func_id, e
                );
                errors.push(format!("解绑 {} 失败: {}", func_id, e));
            } else {
                unbound.push((func_id.clone(), driver_name.to_string()));
            }
        }
    }

    // 解绑阶段出错 → 尝试重新绑定已解绑设备 + PCI rescan 恢复
    if !errors.is_empty() {
        warn!("解绑过程出现错误，尝试恢复已解绑设备...");
        let mut rebind_failures: Vec<String> = Vec::new();
        for (func_id, driver_name) in unbound.iter().rev() {
            let bind_path = format!("/sys/bus/pci/drivers/{}/bind", driver_name);
            if let Err(e) = fs::write(&bind_path, func_id) {
                let msg = format!("恢复绑定 {} 失败: {}", func_id, e);
                warn!("{}", msg);
                rebind_failures.push(msg);
            }
        }
        // 额外尝试 PCI rescan 恢复设备
        if let Err(e) = GpuDetector::rescan_pci_bus() {
            warn!("PCI rescan 恢复失败: {}", e);
        }
        if !rebind_failures.is_empty() {
            log::error!(
                "部分设备重绑定失败！请手动检查 lspci 状态:\n{}",
                rebind_failures.join("\n")
            );
        }
        return Err(FtoolError::Gpu(format!(
            "解绑阶段失败: {} (已尝试恢复)",
            errors.join("; ")
        )));
    }
    Ok(())
}

/// 步骤 2：按功能号降序从 PCI 总线移除设备
///
/// 任一步移除失败时尝试 PCI rescan 恢复设备，随后返回错误（已尝试恢复）。
fn remove_functions(functions: &[String]) -> Result<(), FtoolError> {
    let mut errors: Vec<String> = Vec::new();

    for func_id in functions {
        let remove_path = format!("/sys/bus/pci/devices/{}/remove", func_id);
        debug!("移除 PCI 设备; device={}", func_id);
        if let Err(e) = fs::write(&remove_path, "1") {
            warn!(
                "移除 function 失败，继续处理其他 function; func={}, error={}",
                func_id, e
            );
            errors.push(format!("移除 {} 失败: {}", func_id, e));
        }
    }

    // 移除阶段出错 → 尝试 rescan 恢复设备
    if !errors.is_empty() {
        warn!("移除过程出现错误，尝试 rescan 恢复设备...");
        if let Err(e) = GpuDetector::rescan_pci_bus() {
            warn!("PCI rescan 恢复失败: {}", e);
        }
        return Err(FtoolError::Gpu(format!(
            "移除阶段失败: {} (已尝试 PCI rescan 恢复)",
            errors.join("; ")
        )));
    }
    Ok(())
}

/// 运行时关闭 NVIDIA GPU（无需重启）
pub(super) fn runtime_power_off() -> Result<(), FtoolError> {
    info!("💤 运行时关闭 NVIDIA GPU...");

    // 门禁 1：Nvidia 模式下 NVIDIA 正驱动显示输出，运行时关闭会把主显示热移除
    if GpuDetector::query_current_mode() == GpuMode::Nvidia {
        return Err(FtoolError::Gpu(
            "当前处于 nvidia 模式（NVIDIA 正驱动显示输出），禁止运行时关闭 GPU；\
             请先切换到其他模式并重启后再执行"
                .into(),
        ));
    }

    // 门禁 2：确认无进程正在使用 NVIDIA GPU（无法确认时检查函数报错，不静默放行）
    if has_nvidia_processes()? {
        return Err(FtoolError::Gpu(
            "NVIDIA GPU 上存在运行中的进程，请先终止它们（如 nvidia-smi 查询结果所示）".into(),
        ));
    }

    let pci_id = match GpuDetector::get_nvidia_raw_pci_id() {
        Ok(id) => id,
        Err(_) => {
            // GPU 已不在 sysfs 中（可能已被 udev 移除或从未在线），幂等视为已关闭
            if !GpuDetector::is_nvidia_online() {
                info!("NVIDIA GPU 已处于关闭状态");
                return Ok(());
            }
            return Err(FtoolError::Gpu(
                "无法获取 NVIDIA GPU 的 PCI ID，请检查系统状态".into(),
            ));
        }
    };

    // 按功能号降序收集同 slot 的 NVIDIA function（子设备在前，父设备在后）
    let functions = nvidia_functions_desc(&pci_id)?;

    // 步骤1：解绑所有 NVIDIA 设备的驱动（失败时自动恢复已解绑设备）
    unbind_functions(&functions)?;

    // 步骤2：从 PCI 总线移除设备（失败时自动 rescan 恢复）
    remove_functions(&functions)?;

    Ok(())
}

/// 查询 NVIDIA GPU 电源状态（是否在线）
pub(super) fn query_runtime_power() -> bool {
    GpuDetector::is_nvidia_online()
}

/// 轮询等待 NVIDIA 驱动绑定完成，然后设置 PCI 运行时电源管理
///
/// 参考 system76-power：NVIDIA 驱动初始化后过早修改电源管理属性
/// 可能导致系统锁死。采用轮询 + 超时机制替代固定 sleep：
/// - 先等待驱动绑定（最多 10 秒）
/// - 驱动绑定后再等待 2 秒让 NVIDIA 完成内部初始化
/// - 同步阻塞，避免后台线程因进程退出而夭折
fn apply_power_control(pci_id: &str, mode: GpuMode) -> Result<(), FtoolError> {
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

    let pm = if mode == GpuMode::Nvidia {
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

    // sysfs/kernfs 伪文件写入已直达内核属性回调，无持久化语义；对这类文件
    // 调用 fsync 无意义且部分内核上会返回 EINVAL，导致"写入已生效却报失败"
    file.write_all(pm.as_bytes())
        .map_err(|e| FtoolError::Gpu(format!("设置电源管理失败: {}", e)))?;

    Ok(())
}

/// 自动设置 GPU 电源状态（基于当前模式和 runtimepm 支持判断）
/// 参考 system76-power 的 `auto_power` 逻辑，并按 ftool 的 integrated
/// 实现做了修正：ftool 的 integrated 模式通过 udev 规则物理移除 NVIDIA
/// 设备——设备已不在线时不应再尝试"开启"（rescan 复活后会被移除规则
/// 再次移除，形成竞态且毫无意义），此时保持现状即可。
/// - 非 Integrated 模式 → 开启电源
/// - Integrated 模式且 GPU 在线：支持 runtime PM → 保持在线（空闲自动挂起省电）
/// - Integrated 模式且 GPU 在线：不支持 runtime PM → 关闭电源（与 udev 移除规则目的一致）
/// - Integrated 模式且 GPU 不在线（已被 udev 规则移除）→ 无需操作
pub(super) fn auto_power() -> Result<(), FtoolError> {
    let mode = GpuDetector::query_current_mode();
    if mode == GpuMode::Integrated {
        // GPU 已不在线（integrated 的 udev 规则已物理移除它）：保持现状
        if !GpuDetector::is_nvidia_online() {
            info!("Integrated 模式下 NVIDIA GPU 已被移除/不在线，无需电源操作");
            return Ok(());
        }
        // GPU 仍在线：按 runtimepm 支持决定保持在线（空闲自动挂起）还是关闭
        match GpuDetector::gpu_supports_runtimepm() {
            Ok(true) => runtime_power_on(),
            Ok(false) => runtime_power_off(),
            Err(err) => {
                log::warn!("无法判断 runtimepm 支持，按不支持处理: {}", err);
                runtime_power_off()
            }
        }
    } else {
        runtime_power_on()
    }
}
