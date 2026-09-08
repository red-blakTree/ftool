mod core;
mod features;

use core::error::FtoolError;
use log::error;
use std::ffi::OsString;
use std::io::Write;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn print_usage() {
    println!(
        "ftool - Fedora 系统工具 v{VERSION}
用法:
  ftool -S <内核路径>            签名指定内核文件 (需要 root)
  ftool -U                       系统版本升级 (需要 root)
  ftool -g <操作> [选项]         显卡模式切换与管理 (需要 root)
  ftool -H <算法> <文件>         计算文件哈希值
  ftool -H <算法> -s <字符串>   计算字符串哈希值 (算法: md5, sha1, sha256, sha512)
  (md5/sha1 仅供兼容旧工具，校验用途推荐 sha256/sha512)
  ftool -h                       显示帮助
  ftool -V                       显示版本信息

显卡管理操作:
  integrated   仅使用集成显卡 (省电，屏蔽N卡)
  hybrid       混合模式 (PRIME，按需渲染)
  nvidia       仅使用 NVIDIA 显卡 (高性能)
  default      根据硬件推荐默认模式
  query        查询当前显卡模式
  power [on|off|auto]  运行时电源控制 (无需重启)
  switchable   检测系统是否支持 GPU 切换
  ext-display  检测外接显示器是否需要独显
  runtimepm    检测 GPU 是否支持运行时电源管理
  reset        还原 ftool 做出的所有修改
  cache-create 创建显卡缓存 (在 hybrid 模式下可用)
  cache-delete 删除显卡缓存
  cache-query  查询显卡缓存内容

显卡高级选项 (仅在切换模式时使用):
  --rtd3 [0-3]               在 Hybrid 模式下启用 RTD3 电源管理 (默认值: 2)
  --coolbits [值]            在 Nvidia 模式下启用 Coolbits (默认值: 28)
  --force-comp               在 Nvidia 模式下启用 ForceCompositionPipeline (修复撕裂)
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
        let _ = std::io::stdout().flush(); // exit 不冲刷 stdout，管道场景可能丢失输出
        std::process::exit(1);
    }

    if let Err(e) = run(&args) {
        error!("❌ {e}");
        // 设计取舍：用法错误与运行失败统一以退出码 1 退出（用户主动取消为 0）；
        // 脚本如需区分失败类别，可在此扩展细分退出码
        let _ = std::io::stdout().flush(); // exit 不冲刷 stdout，管道场景可能丢失输出
        std::process::exit(1);
    }
}

/// 顶层命令分发：每个特性的参数解析、权限检查与执行均由特性自身完成
fn run(args: &[OsString]) -> Result<(), FtoolError> {
    match args[1].to_str() {
        Some("-S") => features::signer::handle_command(args),
        Some("-U") => features::upgrader::handle_command(),
        Some("-g") | Some("--graphics") => features::gpu::cli::handle(args),
        Some("-H") => features::hasher::handle_command(args),
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
