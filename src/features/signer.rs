use crate::core::FtoolError;
use crate::core::prompter::Prompter;
use crate::core::runner::CommandRunner;
use log::warn;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
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
    ///
    /// 返回 `Ok(true)` 表示已签名可跳过；`Ok(false)` 表示未签名（或签名不属于当前公钥）。
    /// 验证工具本身执行失败时返回 `Err`，此时不应盲目继续签名，
    /// 避免在验证链路已损坏的情况下仍输出"签名完成"。
    fn is_already_signed(kernel_path: &Path) -> Result<bool, FtoolError> {
        match CommandRunner::run_status(
            "sbverify",
            [
                OsStr::new("--cert"),
                OsStr::new(PUBLIC_KEY),
                kernel_path.as_os_str(),
            ],
        ) {
            Ok(status) => Ok(status.success()),
            Err(e) => Err(FtoolError::Sign(format!(
                "无法执行 sbverify 验证当前签名状态，已中止: {e}"
            ))),
        }
    }

    /// 校验临时签名产物，通过才允许替换原文件
    fn verify_signature(tmp: &Path) -> Result<(), FtoolError> {
        match CommandRunner::run_status(
            "sbverify",
            [
                OsStr::new("--cert"),
                OsStr::new(PUBLIC_KEY),
                tmp.as_os_str(),
            ],
        ) {
            Ok(status) if status.success() => Ok(()),
            Ok(_) => Err(FtoolError::Sign("签名产物未通过 sbverify 校验".into())),
            Err(e) => Err(FtoolError::Sign(format!(
                "无法执行 sbverify 校验签名产物: {e}"
            ))),
        }
    }

    /// 对指定的内核文件执行签名
    ///
    /// 签名流程：
    /// 1. 确保 sbsign 和密钥文件可用
    /// 2. 解析内核文件路径
    /// 3. 幂等性检查：已签名则跳过
    /// 4. 将签名结果写入同目录下的临时文件，并用 sbverify 校验产物
    /// 5. 原文件先原子移为备份，再把签名产物原子替换到位；失败自动回滚
    /// 6. 成功后恢复原文件权限并清理备份
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
        if Self::is_already_signed(&real_path)? {
            println!("⏭️ 内核已持有当前公钥的签名，跳过: {}", real_path.display());
            return Ok(());
        }

        // 覆盖引导文件是不可逆操作，终端交互下先请用户确认
        // （非终端场景跳过确认，与 -U 升级命令的交互约定一致）
        if Prompter::is_terminal()
            && !Prompter::ask_yes(
                &format!(
                    "即将覆盖签名内核文件: {}\n是否继续？ [y/N]: ",
                    real_path.display()
                ),
                false,
            )
        {
            println!("已取消签名。");
            return Ok(());
        }

        let parent_dir = match real_path.parent() {
            Some(dir) => dir,
            None => {
                return Err(FtoolError::Sign(format!(
                    "内核路径 {} 缺少父目录，无法创建临时文件",
                    real_path.display()
                )))
            }
        };

        // sbsign 生成的临时文件权限受 umask 影响（通常 0644），
        // 替换后需恢复原文件权限（如 Fedora vmlinuz 的 0600）
        let original_mode = fs::metadata(&real_path)
            .map(|m| m.permissions().mode())
            .map_err(|e| {
                FtoolError::Sign(format!(
                    "读取内核文件元数据失败 {}: {}",
                    real_path.display(),
                    e
                ))
            })?;

        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let pid = std::process::id();
        let tmp_str = format!(
            "{}/.kernel_sign.tmp.{}-{}",
            parent_dir.display(),
            pid,
            ts
        );
        let bak_str = format!(
            "{}/.kernel_sign.bak.{}-{}",
            parent_dir.display(),
            pid,
            ts
        );
        let tmp = Path::new(&tmp_str);
        let bak = Path::new(&bak_str);

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
                tmp.as_os_str(),
            ],
        ) {
            let _ = fs::remove_file(tmp); // 清理残留临时文件
            return Err(FtoolError::Sign(format!("签名执行失败: {e}")));
        }

        // 替换前校验签名产物，避免把损坏/无效的产物覆盖到引导文件上
        if let Err(e) = Self::verify_signature(tmp) {
            let _ = fs::remove_file(tmp);
            return Err(FtoolError::Sign(format!("{e}，已保留原内核文件")));
        }

        // 确保签名产物落盘，缩小断电留下空/损坏文件的窗口
        fs::File::open(tmp)
            .and_then(|f| f.sync_all())
            .map_err(|e| {
                let _ = fs::remove_file(tmp);
                FtoolError::Sign(format!("同步临时文件到磁盘失败: {e}"))
            })?;

        // 原文件先原子移为备份；后续任一步失败都能从备份回滚
        if let Err(e) = fs::rename(&real_path, bak) {
            let _ = fs::remove_file(tmp);
            return Err(FtoolError::Sign(format!("备份原内核文件失败: {e}")));
        }

        // 临时文件和目标文件在同一分区，fs::rename 保证原子操作
        if let Err(e) = fs::rename(tmp, &real_path) {
            let _ = fs::remove_file(tmp);
            let rollback = fs::rename(bak, &real_path);
            return Err(FtoolError::Sign(format!(
                "替换内核文件失败: {e}；{}",
                if rollback.is_ok() {
                    "已自动恢复原文件".to_string()
                } else {
                    format!("且恢复原文件失败，原文件保留在 {}", bak.display())
                }
            )));
        }

        // 恢复原文件权限（新文件由 sbsign 按 umask 创建）
        if let Err(e) = fs::set_permissions(&real_path, fs::Permissions::from_mode(original_mode))
        {
            warn!("恢复内核文件权限失败: {}", e);
        }

        // 签名成功：清理备份，并同步父目录保证目录项落盘
        if let Err(e) = fs::remove_file(bak) {
            warn!(
                "清理内核备份文件失败（可手动删除）: {}; path={}",
                e,
                bak.display()
            );
        }
        if let Ok(dir) = fs::File::open(parent_dir) {
            let _ = dir.sync_all();
        }

        println!("签名完成: {}", real_path.display());
        Ok(())
    }
}
