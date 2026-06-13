mod core;
mod features;

use core::error::FtoolError;
use log::error;
use std::ffi::OsString;

const VERSION: &str = "0.1.1";

fn print_usage() {
    println!(
        "ftool - Fedora 系统工具 v{VERSION}
用法:
  ftool -S <内核路径>            签名指定内核文件 (需要 root)
  ftool -U                       系统版本升级 (需要 root)
  ftool -g <操作> [选项]         显卡模式切换与管理 (需要 root)
  ftool -H <算法> <文件>         计算文件哈希值
  ftool -H <算法> -s <字符串>   计算字符串哈希值 (算法: md5, sha1, sha256, sha512)
  ftool -h                       显示帮助
  ftool -V                       显示版本信息

显卡管理操作:
  integrated   仅使用集成显卡 (省电，屏蔽N卡)
  compute      集显输出 + N卡计算 (省电+GPU计算)
  hybrid       混合模式 (PRIME，按需渲染)
  nvidia       仅使用 NVIDIA 显卡 (高性能)
  default      根据硬件推荐默认模式
  query        查询当前显卡模式
  power [on|off|auto]  运行时电源控制 (无需重启)
  switchable   检测系统是否支持 GPU 切换
  ext-display  检测外接显示器是否需要独显
  runtimepm    检测 GPU 是否支持运行时电源管理
  reset        还原 ftool 做出的所有修改
  cache-create 创建显卡缓存 (在 hybrid/compute 模式下可用)
  cache-delete 删除显卡缓存
  cache-query  查询显卡缓存内容

显卡高级选项 (仅在切换模式时使用):
  --rtd3 [0-3]               在 Hybrid 模式下启用 RTD3 电源管理 (默认值: 2)
  --coolbits [值]            在 Nvidia 模式下启用 Coolbits (默认值: 28)
  --use-nvidia-current       使用 nvidia-current 内核模块
"
    );
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format(|buf, record| {
            use std::io::Write;
            writeln!(buf, "{}", record.args())
        })
        .init();

    let args: Vec<OsString> = std::env::args_os().collect();
    if args.len() < 2 {
        print_usage();
        std::process::exit(1);
    }

    if let Err(e) = run(&args) {
        error!("❌ {e}");
        std::process::exit(1);
    }
}

fn run(args: &[OsString]) -> Result<(), FtoolError> {
    match args[1].to_str() {
        Some("-S") => handle_sign_command(args),
        Some("-U") => handle_upgrade_command(),
        Some("-g") | Some("--graphics") => features::gpu::cli::handle(args),
        Some("-H") => handle_hash_command(args),
        Some("-h") | Some("--help") => {
            print_usage();
            Ok(())
        }
        Some("-V") | Some("--version") => {
            println!("ftool v{VERSION}");
            Ok(())
        }
        _ => Err(FtoolError::Input(format!(
            "未知参数: {}",
            args[1].to_string_lossy()
        ))),
    }
}

fn handle_sign_command(args: &[OsString]) -> Result<(), FtoolError> {
    if args.len() < 3 {
        return Err(FtoolError::Input("-S 参数需要指定内核文件路径".into()));
    }
    core::privilege::Privilege::ensure_root()
        .and_then(|_| features::signer::KernelSigner::sign_kernel(&args[2]))
}

fn handle_upgrade_command() -> Result<(), FtoolError> {
    core::privilege::Privilege::ensure_root()
        .and_then(|_| features::upgrader::Upgrader::perform_upgrade())
}

fn handle_hash_command(args: &[OsString]) -> Result<(), FtoolError> {
    let algo = args[2].to_string_lossy();

    if args.len() >= 4
        && let Some(flag) = args[3].to_str()
        && (flag == "--string" || flag == "-s")
    {
        // 字符串哈希模式
        if args.len() < 5 {
            return Err(FtoolError::Input(
                "-H --string 参数需要指定要哈希的字符串".into(),
            ));
        }
        let data = args[4].to_string_lossy();
        let hash = features::hasher::Hasher::compute_string(&algo, &data)?;
        println!("{} \"{}\"", hash, data);
        return Ok(());
    }

    // 文件哈希模式（现有行为）
    if args.len() < 4 {
        return Err(FtoolError::Input("-H 参数需要指定算法和文件路径".into()));
    }
    let path = &args[3];
    let hash = features::hasher::Hasher::compute(&algo, path)?;
    println!("{} {}", hash, path.to_string_lossy());
    Ok(())
}


