use crate::core::error::FtoolError;
use crate::core::privilege::Privilege;
use crate::features::gpu::{GpuController, GpuMode, NvidiaOptions, PowerAction, SwitchOptions};
use log::{info, warn};
use std::ffi::OsString;

/// 处理显卡相关命令（从 main.rs 提取，保持参数接口一致）
///
/// `args` 格式：`[<程序名>, "-g", <action>, ...]`
pub fn handle(args: &[OsString]) -> Result<(), FtoolError> {
    if args.len() < 3 {
        return Err(FtoolError::Input(
            "请指定显卡操作或模式\n\
             用法: ftool -g <integrated|compute|hybrid|nvidia|query|power|switchable|reset|...>"
                .into(),
        ));
    }

    let action = args[2].to_string_lossy();

    match action.as_ref() {
        "query" => handle_query(),
        "switchable" => handle_switchable(),
        "cache-query" => handle_cache_query(),
        "default" => handle_default(),
        "ext-display" => handle_ext_display(),
        "runtimepm" => handle_runtimepm(),
        "integrated" | "compute" | "hybrid" | "nvidia" => {
            Privilege::ensure_root()?;
            let opts = parse_switch_options(&action, &args[3..])?;
            GpuController::switch_mode(opts)
        }
        "power" => handle_power(args),
        "reset" => {
            Privilege::ensure_root()?;
            GpuController::reset()
        }
        "cache-create" => {
            Privilege::ensure_root()?;
            GpuController::cache_create()
        }
        "cache-delete" => {
            Privilege::ensure_root()?;
            GpuController::delete_cache()
        }
        other => Err(FtoolError::Input(format!("未知显卡操作: {}", other))),
    }
}

fn handle_query() -> Result<(), FtoolError> {
    println!("{}", GpuController::query_mode().as_str());
    Ok(())
}

fn handle_switchable() -> Result<(), FtoolError> {
    if GpuController::can_switch()? {
        println!("可切换");
    } else {
        println!("不可切换");
    }
    Ok(())
}

fn handle_cache_query() -> Result<(), FtoolError> {
    println!("{}", GpuController::cache_query()?);
    Ok(())
}

fn handle_default() -> Result<(), FtoolError> {
    let mode = GpuController::get_default()?;
    println!("{}", mode.as_str());
    Ok(())
}

fn handle_ext_display() -> Result<(), FtoolError> {
    let requires = GpuController::external_display_requires_nvidia()?;
    if requires {
        println!("需要独显");
    } else {
        println!("不需要独显");
    }
    Ok(())
}

fn handle_runtimepm() -> Result<(), FtoolError> {
    let supports = GpuController::supports_runtimepm()?;
    if supports {
        println!("支持");
    } else {
        println!("不支持");
    }
    Ok(())
}

fn handle_power(args: &[OsString]) -> Result<(), FtoolError> {
    if args.len() <= 3 {
        // 无参数时显示当前状态
        if GpuController::query_power() {
            println!("开启 (独立显卡)");
        } else {
            println!("关闭 (独立显卡)");
        }
        return Ok(());
    }

    Privilege::ensure_root()?;
    let power_action = match args[3].to_string_lossy().as_ref() {
        "on" => PowerAction::On,
        "off" => PowerAction::Off,
        "auto" => PowerAction::Auto,
        other => {
            return Err(FtoolError::Input(format!(
                "不支持的 power 操作: '{}'，仅支持: on, off, auto",
                other
            )));
        }
    };
    GpuController::power(power_action)
}

/// 解析 `--<flag>` 后面的 u32 参数值
///
/// 返回 `(value, new_index_after_consuming)`。
/// 若参数缺失或格式无效则返回错误。
fn parse_u32_flag(args: &[OsString], i: usize, flag: &str) -> Result<(u32, usize), FtoolError> {
    if i + 1 < args.len() {
        match args[i + 1].to_string_lossy().parse::<u32>() {
            Ok(v) => Ok((v, i + 2)),
            Err(_) => Err(FtoolError::Input(format!(
                "--{} 的值 '{}' 不是有效数字",
                flag,
                args[i + 1].to_string_lossy()
            ))),
        }
    } else {
        Err(FtoolError::Input(format!("--{} 需要指定值", flag)))
    }
}

/// 解析显卡切换的高级选项参数
fn parse_switch_options(
    mode: &str,
    args: &[OsString],
) -> Result<SwitchOptions, FtoolError> {
    let gpu_mode = mode.parse::<GpuMode>()?;
    let mut nv_opts = NvidiaOptions::default();

    let mut i = 0;
    while i < args.len() {
        let arg = args[i].to_string_lossy();
        match arg.as_ref() {
            "--coolbits" => {
                let (val, next) = parse_u32_flag(args, i, "coolbits")?;
                if val > 31 {
                    return Err(FtoolError::Input(format!(
                        "Coolbits 值必须在 0-31 之间（5-bit 位掩码），当前值: {}",
                        val
                    )));
                }
                nv_opts.coolbits = Some(val);
                i = next;
            }
            "--rtd3" => {
                let (val, next) = parse_u32_flag(args, i, "rtd3")?;
                if val > 3 {
                    return Err(FtoolError::Input("RTD3 值必须在 0-3 之间".into()));
                }
                nv_opts.rtd3 = Some(val);
                i = next;
            }
            "--use-nvidia-current" => {
                nv_opts.use_nvidia_current = true;
                i += 1;
            }
            "--force-comp" => {
                nv_opts.force_comp = true;
                i += 1;
            }
            _ => return Err(FtoolError::Input(format!("未知参数: {}", arg))),
        }
    }

    info!(
        "解析显卡切换参数完成; mode={}, coolbits={:?}, rtd3={:?}, use_nvidia_current={}, force_comp={}",
        gpu_mode.as_str(),
        nv_opts.coolbits,
        nv_opts.rtd3,
        nv_opts.use_nvidia_current,
        nv_opts.force_comp,
    );

    // 非 nvidia 模式下使用 --coolbits 时发出警告
    if gpu_mode != GpuMode::Nvidia && nv_opts.coolbits.is_some() {
        warn!(
            "--coolbits 仅在 nvidia 模式下生效，当前 {} 模式将忽略该选项",
            gpu_mode.as_str()
        );
    }

    // 非 nvidia 模式下使用 --force-comp 时发出警告
    if gpu_mode != GpuMode::Nvidia && nv_opts.force_comp {
        warn!(
            "--force-comp 仅在 nvidia 模式下生效，当前 {} 模式将忽略该选项",
            gpu_mode.as_str()
        );
    }

    Ok(SwitchOptions {
        mode: gpu_mode,
        nvidia_opts: nv_opts,
    })
}
