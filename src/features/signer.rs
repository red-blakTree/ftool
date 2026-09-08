use crate::core::FtoolError;
use crate::core::privilege::Privilege;
use crate::core::prompter::{NonTerminal, Prompter};
use crate::core::runner::CommandRunner;
use log::warn;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
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
        // 自动安装会触发联网与系统包变更：仅在交互终端经用户确认后执行，
        // 非终端场景（脚本/cron）直接给出手动安装指引
        if !Prompter::confirm(
            "📦 未找到 sbsign 命令，是否自动安装 sbsigntools？ [y/N]: ",
            false,
            NonTerminal::Deny,
        ) {
            return Err(FtoolError::Sign(
                "未找到 sbsign 命令，请先手动安装: sudo dnf install sbsigntools".into(),
            ));
        }
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

    /// 使用当前公钥执行 sbverify 校验；`Ok(true)` 表示校验通过。
    ///
    /// 校验工具本身执行失败时返回 `Err`——此时不应盲目继续签名/替换，
    /// 避免在验证链路已损坏的情况下仍输出"签名完成"。
    fn sbverify_passes(path: &Path) -> Result<bool, FtoolError> {
        match CommandRunner::run_status(
            "sbverify",
            [
                OsStr::new("--cert"),
                OsStr::new(PUBLIC_KEY),
                path.as_os_str(),
            ],
        ) {
            Ok(status) => Ok(status.success()),
            Err(e) => Err(FtoolError::Sign(format!(
                "无法执行 sbverify 校验 {}: {e}",
                path.display()
            ))),
        }
    }

    /// 提交阶段（原文件已移为备份后）任一步骤失败的统一收尾：
    /// 删除临时签名产物，把备份恢复回原路径，并组装带恢复结果的错误。
    ///
    /// 调用点包括：设置签名产物权限失败、权限元数据落盘失败、替换原文件失败。
    fn commit_failed(
        tmp: &Path,
        bak: &Path,
        real_path: &Path,
        step: &str,
        cause: impl std::fmt::Display,
    ) -> FtoolError {
        let _ = fs::remove_file(tmp); // 清理临时签名产物
        let rollback = fs::rename(bak, real_path);
        // 回滚成功后同步父目录，确保"备份移回原路径"的目录项落盘（尽力而为）
        if rollback.is_ok()
            && let Some(parent) = real_path.parent()
        {
            Self::sync_dir(parent);
        }
        FtoolError::Sign(format!(
            "{step}失败: {cause}；{}",
            if rollback.is_ok() {
                "已自动恢复原文件".to_string()
            } else {
                format!("且恢复原文件失败，原文件保留在 {}", bak.display())
            }
        ))
    }

    /// 同步目录的目录项到磁盘（Linux 允许 fsync 目录；尽力而为，失败仅告警）。
    ///
    /// rename/remove 等目录项变更需目录 fsync 才能在断电/崩溃后持久可见，
    /// 但目录 fsync 失败不应阻断签名主流程，故只记录告警、不返回错误。
    fn sync_dir(dir: &Path) {
        if let Err(e) = fs::File::open(dir).and_then(|d| d.sync_all()) {
            warn!(
                "同步目录到磁盘失败（尽力而为）: {}; path={}",
                e,
                dir.display()
            );
        }
    }

    /// 以 O_EXCL（create_new）独占创建签名临时输出文件的占位普通文件。
    ///
    /// tmp 文件名（pid-时间戳）可预测，sbsign 以 O_TRUNC 打开 --output 会跟随
    /// 符号链接：root 在攻击者可写的目录上执行 -S 时，预置同名符号链接可诱导
    /// 覆写任意文件（/boot 等 root 独占目录不受影响）。先独占创建占位文件可使
    /// 预置的符号链接/文件直接撞 EEXIST 报错而非被跟随（参考 file_io.rs 的原子
    /// 写做法：创建期权限收紧到 0600）。假定 sbsign 对已存在的 --output 以
    /// O_TRUNC 正常覆写（主流签名工具行为）；真实环境无法运行 sbsign 验证时
    /// 本路径保持自洽——若 sbsign 反而要求输出文件不存在，会以清晰的命令错误
    /// 失败，不会静默产出错误产物。
    fn create_tmp_placeholder(tmp: &Path) -> Result<(), FtoolError> {
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create_new(true);
        opts.mode(0o600);
        match opts.open(tmp) {
            Ok(_) => Ok(()),
            Err(e) => {
                // 创建失败时 tmp 可能并非本进程创建（预置文件/链接），不擅自删除
                Err(FtoolError::Sign(format!(
                    "创建签名临时输出文件失败 {}: {}",
                    tmp.display(),
                    e
                )))
            }
        }
    }

    /// 校验签名产物仍为普通文件（非符号链接），不是普通文件则清理并报错。
    ///
    /// 预创建只挡住"开始时"的预置链接；签名过程中若目录可写者把占位文件替换
    /// 成符号链接，后续 fs::set_permissions 会跟随链接作用于其指向的目标。
    /// rename 本身不跟随符号链接，此校验把竞态压到残留的极小窗口。
    fn ensure_tmp_regular(tmp: &Path) -> Result<(), FtoolError> {
        match fs::symlink_metadata(tmp) {
            Ok(md) if md.file_type().is_file() => Ok(()),
            Ok(_) => {
                let _ = fs::remove_file(tmp);
                Err(FtoolError::Sign(format!(
                    "签名产物 {} 不是普通文件（疑似被符号链接替换），已中止",
                    tmp.display()
                )))
            }
            Err(e) => {
                let _ = fs::remove_file(tmp);
                Err(FtoolError::Sign(format!(
                    "读取签名产物元数据失败 {}: {}",
                    tmp.display(),
                    e
                )))
            }
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
        if Self::sbverify_passes(&real_path)? {
            println!("⏭️ 内核已持有当前公钥的签名，跳过: {}", real_path.display());
            return Ok(());
        }

        // 覆盖引导文件是不可逆操作，非交互终端（脚本/cron）直接中止：
        // 既不静默放行、也不静默走"取消"——与 upgrader 的 confirm_upgrade
        // 行为对齐，危险操作必须由人工在终端确认
        if !Prompter::is_terminal() {
            return Err(FtoolError::Sign(
                "stdin 不是交互终端，已中止签名（覆盖引导文件必须人工在终端确认）".into(),
            ));
        }
        if !Prompter::ask_yes(
            &format!(
                "即将覆盖签名内核文件: {}\n是否继续？ [y/N]: ",
                real_path.display()
            ),
            false,
        ) {
            println!("已取消签名。");
            return Ok(());
        }

        let parent_dir = match real_path.parent() {
            Some(dir) => dir,
            None => {
                return Err(FtoolError::Sign(format!(
                    "内核路径 {} 缺少父目录，无法创建临时文件",
                    real_path.display()
                )));
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
        let tmp_str = format!("{}/.kernel_sign.tmp.{}-{}", parent_dir.display(), pid, ts);
        let bak_str = format!("{}/.kernel_sign.bak.{}-{}", parent_dir.display(), pid, ts);
        let tmp = Path::new(&tmp_str);
        let bak = Path::new(&bak_str);
        // 防符号链接竞态（见 create_tmp_placeholder）：sbsign 前先独占创建占位文件
        Self::create_tmp_placeholder(tmp)?;

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
        // 防符号链接竞态第二道校验：签名完成后确认 tmp 仍是普通文件再继续提交
        Self::ensure_tmp_regular(tmp)?;
        // 替换前校验签名产物，避免把损坏/无效的产物覆盖到引导文件上
        if !Self::sbverify_passes(tmp).inspect_err(|_| {
            let _ = fs::remove_file(tmp);
        })? {
            let _ = fs::remove_file(tmp);
            return Err(FtoolError::Sign(
                "签名产物未通过 sbverify 校验，已保留原内核文件".into(),
            ));
        }

        // 确保签名产物落盘，缩小断电留下空/损坏文件的窗口
        fs::File::open(tmp)
            .and_then(|f| f.sync_all())
            .map_err(|e| {
                let _ = fs::remove_file(tmp);
                FtoolError::Sign(format!("同步临时文件到磁盘失败: {e}"))
            })?;

        // —— 提交阶段 ——
        // 崩溃一致性说明：rename(real→bak) 与 rename(tmp→real) 之间仍存在极小
        // 窗口（断电时盘上仅剩备份、real 缺失）。未使用 renameat2(RENAME_EXCHANGE)
        // 原子互换：std 未封装、需直接 syscall，暂不引入——属已知取舍；本文件通过
        // 两处目录 fsync 把"原文件丢失"的主要风险压到最小
        // 原文件先原子移为备份；后续任一步失败都能从备份回滚
        if let Err(e) = fs::rename(&real_path, bak) {
            let _ = fs::remove_file(tmp);
            return Err(FtoolError::Sign(format!("备份原内核文件失败: {e}")));
        }
        // "原文件已移为备份"的目录项立即落盘：若此后断电/崩溃，备份文件在目录
        // 重启后依然可见，可据此找回原文件
        Self::sync_dir(parent_dir);
        // 在替换前把签名产物权限设为原文件权限（sbsign 按 umask 创建，可能丢失
        // 0600 等收紧权限）：rename 后新文件直接以目标权限可见，不存在"先 0644
        // 落盘、再事后收窄"的瞬时放宽窗口；失败时回滚备份并中止
        if let Err(e) = fs::set_permissions(tmp, fs::Permissions::from_mode(original_mode)) {
            return Err(Self::commit_failed(
                tmp,
                bak,
                &real_path,
                "设置签名产物权限",
                e,
            ));
        }
        // 权限元数据也需落盘后再 rename（首次 sync 发生在设置权限之前）
        fs::File::open(tmp)
            .and_then(|f| f.sync_all())
            .map_err(|e| Self::commit_failed(tmp, bak, &real_path, "同步签名产物权限", e))?;

        // 临时文件和目标文件在同一分区，fs::rename 保证原子操作
        if let Err(e) = fs::rename(tmp, &real_path) {
            return Err(Self::commit_failed(tmp, bak, &real_path, "替换内核文件", e));
        }

        // 签名成功：清理备份，并同步父目录，保证"替换"与"清理备份"的目录项落盘
        if let Err(e) = fs::remove_file(bak) {
            warn!(
                "清理内核备份文件失败（可手动删除）: {}; path={}",
                e,
                bak.display()
            );
        }
        Self::sync_dir(parent_dir);

        println!("签名完成: {}", real_path.display());
        Ok(())
    }
}

/// 处理 `-S` 命令：参数校验 + root 检查后执行内核签名
pub fn handle_command(args: &[OsString]) -> Result<(), FtoolError> {
    if args.len() < 3 {
        return Err(FtoolError::Input("-S 参数需要指定内核文件路径".into()));
    }
    if args.len() > 3 {
        return Err(FtoolError::Input(
            "-S 只接受一个内核文件路径参数（多余参数；路径含空格请用引号包裹）".into(),
        ));
    }
    Privilege::ensure_root().and_then(|_| KernelSigner::sign_kernel(&args[2]))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------- commit_failed：提交阶段失败的统一收尾 ----------

    /// 成功场景：临时产物被清理，备份回滚到原路径，错误消息带恢复说明
    #[test]
    fn commit_failed_cleans_tmp_and_rolls_back_bak() {
        let dir = std::env::temp_dir().join(format!("ftool-sign-rollback-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tmp = dir.join("tmp");
        let bak = dir.join("bak");
        let real = dir.join("real");
        std::fs::write(&tmp, b"signed").unwrap();
        std::fs::write(&bak, b"original").unwrap();

        let err = KernelSigner::commit_failed(
            &tmp,
            &bak,
            &real,
            "设置签名产物权限",
            std::io::Error::other("模拟失败"),
        );

        assert!(!tmp.exists(), "临时签名产物应被清理");
        assert!(!bak.exists(), "备份应已被移回原路径");
        assert_eq!(
            std::fs::read_to_string(&real).unwrap(),
            "original",
            "原文件应从备份恢复"
        );
        match err {
            FtoolError::Sign(msg) => {
                assert!(msg.contains("设置签名产物权限失败"));
                assert!(msg.contains("已自动恢复原文件"));
            }
            other => panic!("期望 Sign 错误，实际: {other:?}"),
        }

        let _ = std::fs::remove_file(&real);
        let _ = std::fs::remove_dir(&dir);
    }

    /// 回滚失败场景（备份缺失）：错误消息应指明恢复失败并保留备份路径信息
    #[test]
    fn commit_failed_reports_when_rollback_impossible() {
        let dir = std::env::temp_dir().join(format!("ftool-sign-nobak-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tmp = dir.join("tmp");
        let bak = dir.join("missing_bak"); // 故意不创建：回滚将失败
        let real = dir.join("real");
        std::fs::write(&tmp, b"signed").unwrap();

        let err = KernelSigner::commit_failed(&tmp, &bak, &real, "替换内核文件", "磁盘错误");

        assert!(!tmp.exists(), "临时签名产物仍应被清理");
        assert!(!real.exists(), "回滚失败时原路径不应出现文件");
        match err {
            FtoolError::Sign(msg) => {
                assert!(msg.contains("替换内核文件失败"));
                assert!(msg.contains("且恢复原文件失败"));
                assert!(msg.contains("missing_bak"), "消息应包含备份路径: {msg}");
            }
            other => panic!("期望 Sign 错误，实际: {other:?}"),
        }

        let _ = std::fs::remove_dir(&dir);
    }

    // ---------- 临时输出占位文件：符号链接竞态缓解 ----------

    /// WHY: tmp 文件名可预测，sbsign 会跟随 --output 上的符号链接——
    /// create_new 预创建必须让预置符号链接撞 EEXIST 报错，而不是跟随它覆写目标
    #[test]
    fn create_tmp_placeholder_rejects_preset_symlink() {
        let dir =
            std::env::temp_dir().join(format!("ftool-sign-placeholder-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let victim = dir.join("victim");
        std::fs::write(&victim, b"precious").unwrap();
        let tmp = dir.join(".kernel_sign.tmp.preset");
        std::os::unix::fs::symlink(&victim, &tmp).unwrap();

        let err = KernelSigner::create_tmp_placeholder(&tmp).unwrap_err();

        match err {
            FtoolError::Sign(msg) => {
                assert!(msg.contains("创建签名临时输出文件失败"), "{msg}");
                assert!(
                    msg.contains(&tmp.display().to_string()),
                    "错误应包含 tmp 路径: {msg}"
                );
            }
            other => panic!("期望 Sign 错误，实际: {other:?}"),
        }
        // 预置符号链接未被删除，指向的目标未被触碰
        assert!(
            std::fs::symlink_metadata(&tmp)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "precious");

        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_file(&victim);
        let _ = std::fs::remove_dir(&dir);
    }

    /// 占位文件创建成功时应是权限 0600 的普通空文件（创建期权限收紧）
    #[test]
    fn create_tmp_placeholder_creates_private_regular_file() {
        let dir = std::env::temp_dir().join(format!("ftool-sign-tmpfile-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tmp = dir.join(".kernel_sign.tmp.fresh");

        KernelSigner::create_tmp_placeholder(&tmp).unwrap();

        let md = std::fs::metadata(&tmp).unwrap();
        assert!(md.is_file(), "占位文件应为普通文件");
        assert_eq!(md.len(), 0, "占位文件应为空文件（内容由 sbsign 写入）");
        assert_eq!(
            md.permissions().mode() & 0o777,
            0o600,
            "创建期权限应收紧到 0600"
        );

        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_dir(&dir);
    }

    /// WHY: 签名成功后若占位文件被替换为符号链接，后续 set_permissions 会跟随
    /// 链接作用于其指向的目标——提交前必须复核 tmp 仍是普通文件
    #[test]
    fn ensure_tmp_regular_rejects_symlink_and_missing() {
        let dir = std::env::temp_dir().join(format!("ftool-sign-regular-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let victim = dir.join("victim");
        std::fs::write(&victim, b"x").unwrap();
        let tmp = dir.join(".kernel_sign.tmp");
        std::os::unix::fs::symlink(&victim, &tmp).unwrap();

        match KernelSigner::ensure_tmp_regular(&tmp) {
            Err(FtoolError::Sign(msg)) => assert!(msg.contains("不是普通文件"), "{msg}"),
            other => panic!("期望 Sign 错误，实际: {other:?}"),
        }
        // 链接应被清理，且目标文件未被修改
        assert!(!tmp.exists(), "符号链接应被清理");
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "x");

        // 普通文件应放行；不存在的路径应报错
        std::fs::remove_file(&victim).unwrap();
        let ok = dir.join(".kernel_sign.tmp.ok");
        std::fs::write(&ok, b"signed").unwrap();
        assert!(KernelSigner::ensure_tmp_regular(&ok).is_ok());
        assert!(KernelSigner::ensure_tmp_regular(&dir.join(".kernel_sign.tmp.missing")).is_err());

        let _ = std::fs::remove_file(&ok);
        let _ = std::fs::remove_dir(&dir);
    }
}
