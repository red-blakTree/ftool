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
             用法: ftool -g <integrated|hybrid|nvidia|query|power|switchable|reset|...>"
                .into(),
        ));
    }

    let action = args[2].to_string_lossy();

    // 无参数子命令不允许携带多余参数：与 power 子命令对多余参数显式报错的行为
    // 对齐，避免 `ftool -g query extra` 这类笔误/脚本残留参数被静默忽略
    if matches!(
        action.as_ref(),
        "query"
            | "switchable"
            | "cache-query"
            | "default"
            | "ext-display"
            | "runtimepm"
            | "reset"
            | "cache-create"
            | "cache-delete"
    ) && args.len() > 3
    {
        return Err(FtoolError::Input(format!(
            "多余参数: {action} 子命令不接受参数"
        )));
    }

    match action.as_ref() {
        "query" => handle_query(),
        "switchable" => handle_switchable(),
        "cache-query" => handle_cache_query(),
        "default" => handle_default(),
        "ext-display" => handle_ext_display(),
        "runtimepm" => handle_runtimepm(),
        "integrated" | "hybrid" | "nvidia" => {
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

    if args.len() > 4 {
        return Err(FtoolError::Input(
            "power 子命令最多接受一个参数 (on|off|auto)".into(),
        ));
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
fn parse_switch_options(mode: &str, args: &[OsString]) -> Result<SwitchOptions, FtoolError> {
    let gpu_mode = mode.parse::<GpuMode>()?;
    let mut nv_opts = NvidiaOptions::default();

    let mut i = 0;
    while i < args.len() {
        let arg = args[i].to_string_lossy();
        match arg.as_ref() {
            "--coolbits" => {
                // 同一选项只允许出现一次：重复指定多半是脚本笔误，静默后值覆盖
                // 前值（旧行为）会让用户误以为生效的是第一个值
                if nv_opts.coolbits.is_some() {
                    return Err(FtoolError::Input("参数重复指定: --coolbits".into()));
                }
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
                if nv_opts.rtd3.is_some() {
                    return Err(FtoolError::Input("参数重复指定: --rtd3".into()));
                }
                let (val, next) = parse_u32_flag(args, i, "rtd3")?;
                if val > 3 {
                    return Err(FtoolError::Input("RTD3 值必须在 0-3 之间".into()));
                }
                nv_opts.rtd3 = Some(val);
                i = next;
            }
            "--use-nvidia-current" => {
                if nv_opts.use_nvidia_current {
                    return Err(FtoolError::Input(
                        "参数重复指定: --use-nvidia-current".into(),
                    ));
                }
                nv_opts.use_nvidia_current = true;
                i += 1;
            }
            "--force-comp" => {
                if nv_opts.force_comp {
                    return Err(FtoolError::Input("参数重复指定: --force-comp".into()));
                }
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

    // --rtd3 仅在 hybrid 模式下生效，其他模式静默忽略会让用户误以为已启用
    if gpu_mode != GpuMode::Hybrid && nv_opts.rtd3.is_some() {
        warn!(
            "--rtd3 仅在 hybrid 模式下生效，当前 {} 模式将忽略该选项",
            gpu_mode.as_str()
        );
    }

    // --use-nvidia-current 在 integrated 模式下无意义（NVIDIA 模块已被黑名单）
    if gpu_mode == GpuMode::Integrated && nv_opts.use_nvidia_current {
        warn!(
            "--use-nvidia-current 在 integrated 模式下无效（NVIDIA 模块已被黑名单），将忽略该选项"
        );
    }

    Ok(SwitchOptions {
        mode: gpu_mode,
        nvidia_opts: nv_opts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 组装 handle() 风格参数列表
    fn os_args(parts: &[&str]) -> Vec<OsString> {
        parts.iter().map(OsString::from).collect()
    }

    /// 断言解析返回 Input 错误且消息包含指定片段
    fn assert_parse_input_err(mode: &str, args: &[&str], expect: &str) {
        let err = match parse_switch_options(mode, &os_args(args)) {
            Err(e) => e,
            Ok(_) => panic!("应返回错误: {}", args.join(" ")),
        };
        let msg = err.to_string();
        assert!(msg.contains(expect), "错误消息应包含 '{expect}': {msg}");
    }

    // ---------- parse_switch_options：重复 flag 显式报错 ----------

    /// WHY: `--coolbits 5 --coolbits 12` 这类重复指定多半是脚本笔误；旧行为
    /// 静默以后值覆盖前值，用户以为生效的是第一个值，笔误难以被察觉
    #[test]
    fn duplicate_coolbits_is_rejected() {
        assert_parse_input_err(
            "nvidia",
            &["--coolbits", "5", "--coolbits", "12"],
            "参数重复指定: --coolbits",
        );
    }

    #[test]
    fn duplicate_rtd3_is_rejected() {
        assert_parse_input_err(
            "hybrid",
            &["--rtd3", "1", "--rtd3", "2"],
            "参数重复指定: --rtd3",
        );
    }

    /// WHY: 布尔开关重复指定同样属于参数错误（重复 --flag 说明调用方拼装出错）
    #[test]
    fn duplicate_boolean_flags_are_rejected() {
        for flag in ["--use-nvidia-current", "--force-comp"] {
            assert_parse_input_err("nvidia", &[flag, flag], &format!("参数重复指定: {flag}"));
        }
    }

    /// 回归：单次指定各选项仍应解析成功，重复检查不应误伤合法调用
    #[test]
    fn single_occurrence_of_each_flag_still_parses() {
        let opts = parse_switch_options("nvidia", &os_args(&["--coolbits", "12"])).unwrap();
        assert_eq!(opts.nvidia_opts.coolbits, Some(12));
        let opts = parse_switch_options("hybrid", &os_args(&["--rtd3", "2"])).unwrap();
        assert_eq!(opts.nvidia_opts.rtd3, Some(2));
    }

    // ---------- 无参数子命令：多余参数显式报错 ----------

    /// WHY: 无参数子命令此前静默忽略多余参数，会掩盖笔误或脚本残留参数；
    /// power 子命令已对多余参数报错，此处把其余无参子命令的行为统一对齐
    #[test]
    fn no_arg_subcommands_reject_extra_args() {
        for action in [
            "query",
            "switchable",
            "cache-query",
            "default",
            "ext-display",
            "runtimepm",
            "reset",
            "cache-create",
            "cache-delete",
        ] {
            let args = os_args(&["ftool", "-g", action, "extra"]);
            let err = match handle(&args) {
                Err(e) => e,
                Ok(_) => panic!("{action} 携带多余参数应报错"),
            };
            let msg = err.to_string();
            assert!(msg.contains("多余参数") && msg.contains(action), "{msg}");
        }
    }
}
