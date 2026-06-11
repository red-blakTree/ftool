use crate::core::FtoolError;
use crate::core::runner::CommandRunner;
use std::ffi::OsStr;
use std::fs;
use std::path::Path;

/// 内核签名私钥路径
const PRIVATE_KEY: &str = "/etc/pki/akmods/private/private_key.priv";
/// 内核签名公钥路径
const PUBLIC_KEY: &str = "/etc/pki/akmods/certs/public_key.pem";

/// 内核签名工具（使用 sbsign）
pub struct KernelSigner;

impl KernelSigner {
    /// 确保 sbsign 命令可用，不可用时自动安装 sbsigntools
    fn ensure_sbsign_available() -> Result<(), FtoolError> {
        let status = CommandRunner::run_status("which", [OsStr::new("sbsign")])?;
        if status.success() {
            return Ok(());
        }
        println!("📦 未找到 sbsign 命令，正在自动安装 sbsigntools...");
        let install_status = CommandRunner::run_status("dnf", ["install", "-y", "sbsigntools"])?;
        if !install_status.success() {
            return Err(FtoolError::Sign(
                "自动安装 sbsigntools 失败，请检查网络或确认是否有 root 权限".into(),
            ));
        }
        let recheck = CommandRunner::run_status("which", [OsStr::new("sbsign")])?;
        if !recheck.success() {
            return Err(FtoolError::Sign(
                "sbsigntools 安装完成，但仍未找到 sbsign 命令".into(),
            ));
        }
        println!("✅ sbsigntools 安装成功");
        Ok(())
    }

    /// 检查签名所需的公私钥文件是否存在
    fn ensure_keys_exist() -> Result<(), FtoolError> {
        if !Path::new(PRIVATE_KEY).exists() {
            return Err(FtoolError::Sign(format!("私钥文件不存在: {PRIVATE_KEY}")));
        }
        if !Path::new(PUBLIC_KEY).exists() {
            return Err(FtoolError::Sign(format!("公钥文件不存在: {PUBLIC_KEY}")));
        }
        Ok(())
    }

    /// 检查内核是否已使用当前公钥签名（幂等性保护，防止重复签名）
    fn is_already_signed(kernel_path: &Path) -> bool {
        let status = CommandRunner::run_status(
            "sbverify",
            [
                OsStr::new("--cert"),
                OsStr::new(PUBLIC_KEY),
                kernel_path.as_os_str(),
            ],
        );
        matches!(status, Ok(s) if s.success())
    }

    /// 对指定的内核文件执行签名
    ///
    /// 签名流程：
    /// 1. 确保 sbsign 和密钥文件可用
    /// 2. 解析内核文件路径
    /// 3. 幂等性检查：已签名则跳过
    /// 4. 将签名结果写入同目录下的临时文件
    /// 5. 原子替换原内核文件（fs::rename 在同一分区下保证原子性）
    pub fn sign_kernel(path: &OsStr) -> Result<(), FtoolError> {
        Self::ensure_sbsign_available()?;
        Self::ensure_keys_exist()?;

        let real_path = fs::canonicalize(path).map_err(|e| {
            FtoolError::Sign(format!(
                "无法解析内核路径 '{}': {}",
                path.to_string_lossy(),
                e
            ))
        })?;

        // 幂等性检查：如果已经签名，直接跳过
        if Self::is_already_signed(&real_path) {
            println!("⏭️ 内核已持有当前公钥的签名，跳过: {}", real_path.display());
            return Ok(());
        }

        let parent_dir = real_path.parent().expect("内核路径必须包含父目录");
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let tmp_str = format!(
            "{}/.kernel_sign.tmp.{}-{}",
            parent_dir.display(),
            std::process::id(),
            ts
        );
        let tmp = Path::new(&tmp_str);

        println!("正在签名: {}", real_path.display());

        // 使用 run_checked 替代 run_status，以便在 sbsign 失败时自动捕获 stderr 中的详细错误信息
        if let Err(e) = CommandRunner::run_checked(
            "sbsign",
            [
                OsStr::new("--key"),
                OsStr::new(PRIVATE_KEY),
                OsStr::new("--cert"),
                OsStr::new(PUBLIC_KEY),
                real_path.as_os_str(),
                OsStr::new("--output"),
                OsStr::new(&tmp_str),
            ],
        ) {
            let _ = fs::remove_file(tmp); // 清理残留临时文件
            return Err(FtoolError::Sign(format!("签名执行失败: {e}")));
        }

        // 临时文件和目标文件在同一分区，fs::rename 保证原子操作
        if let Err(e) = fs::rename(tmp, &real_path) {
            let _ = fs::remove_file(tmp);
            return Err(FtoolError::Sign(format!("替换内核文件失败: {e}")));
        }

        println!("签名完成: {}", real_path.display());
        Ok(())
    }
}
