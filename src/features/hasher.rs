use crate::core::FtoolError;
use digest::Digest;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{BufReader, Cursor, Read};

/// 文件哈希值计算器
pub struct Hasher;

impl Hasher {
    /// 计算指定文件的哈希值
    ///
    /// # 参数
    /// * `algo` - 哈希算法名称，支持: md5, sha1, sha256, sha512
    /// * `path` - 文件路径
    ///
    /// # 返回
    /// 小写十六进制字符串表示的哈希值
    pub fn compute(algo: &str, path: &OsStr) -> Result<String, FtoolError> {
        let file = File::open(path).map_err(|e| {
            FtoolError::File(format!("无法打开文件 '{}': {}", path.to_string_lossy(), e))
        })?;
        const BUF_SIZE: usize = 1024 * 1024; // 1MiB 缓冲区
        let mut reader = BufReader::with_capacity(BUF_SIZE, file);

        Self::dispatch(algo, &mut reader)
    }

    /// 计算字符串的哈希值
    ///
    /// # 参数
    /// * `algo` - 哈希算法名称，支持: md5, sha1, sha256, sha512
    /// * `data` - 要计算哈希的字符串
    ///
    /// # 返回
    /// 小写十六进制字符串表示的哈希值
    pub fn compute_string(algo: &str, data: &str) -> Result<String, FtoolError> {
        let mut cursor = Cursor::new(data.as_bytes());
        Self::dispatch(algo, &mut cursor)
    }

    /// 根据算法名称分发到对应的哈希实现
    fn dispatch<D: Read>(algo: &str, reader: &mut D) -> Result<String, FtoolError> {
        match algo {
            a if a.eq_ignore_ascii_case("md5") => Self::hash::<md5::Md5>(reader),
            a if a.eq_ignore_ascii_case("sha1") => Self::hash::<sha1::Sha1>(reader),
            a if a.eq_ignore_ascii_case("sha256") => Self::hash::<sha2::Sha256>(reader),
            a if a.eq_ignore_ascii_case("sha512") => Self::hash::<sha2::Sha512>(reader),
            _ => Err(FtoolError::Input(format!(
                "不支持的哈希算法: {algo}，支持: md5, sha1, sha256, sha512"
            ))),
        }
    }

    /// 使用指定的摘要算法计算哈希值
    ///
    /// 以 1MB 块为单位读取输入流，并输出小写十六进制字符串。
    fn hash<D: Digest>(reader: &mut impl Read) -> Result<String, FtoolError> {
        let mut hasher = D::new();
        const BUF_SIZE: usize = 1024 * 1024; // 1MiB 缓冲区
        let mut buffer = vec![0u8; BUF_SIZE];
        loop {
            let n = match reader.read(&mut buffer) {
                // EINTR（信号中断）属可重试错误：继续下一轮读取。大文件的多次
                // read 中被信号打断的概率不低，直接返回错误会让一次无害信号
                // 中断整个哈希计算
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(FtoolError::File(format!("读取文件失败: {}", e))),
                Ok(n) => n,
            };
            if n == 0 {
                break;
            }
            hasher.update(&buffer[..n]);
        }

        const HEX_CHARS: &[u8; 16] = b"0123456789abcdef";
        let result = hasher.finalize();
        let mut hex_bytes = Vec::with_capacity(result.len() * 2);
        for &byte in result.iter() {
            hex_bytes.push(HEX_CHARS[(byte >> 4) as usize]);
            hex_bytes.push(HEX_CHARS[(byte & 0x0f) as usize]);
        }
        // 不变量：hex_bytes 仅由 HEX_CHARS 的 ASCII 十六进制字符组成，
        // UTF-8 转换不可能失败
        Ok(String::from_utf8(hex_bytes).expect("hex 输出仅含 ASCII，UTF-8 转换不可能失败"))
    }
}

/// 判断算法名是否在受支持白名单内（大小写不敏感）
///
/// 白名单与 dispatch 的匹配规则一致；handle_command 在打开文件前先调用本函数
/// 做用法校验，dispatch 内部仍保留自身的防御性校验
fn is_supported_algo(algo: &str) -> bool {
    ["md5", "sha1", "sha256", "sha512"]
        .iter()
        .any(|a| a.eq_ignore_ascii_case(algo))
}

/// 处理 `-H` 命令：解析算法与文件/字符串参数，计算并打印哈希
pub fn handle_command(args: &[OsString]) -> Result<(), FtoolError> {
    if args.len() < 3 {
        return Err(FtoolError::Input("-H 参数需要指定算法".into()));
    }
    let algo = args[2].to_string_lossy();

    // 算法白名单校验前置（在打开文件之前）：若先打开文件再校验算法，
    // -H 拼错算法且文件路径不存在时会报误导性的"无法打开文件"，掩盖用法错误
    if !is_supported_algo(&algo) {
        return Err(FtoolError::Input(format!(
            "不支持的哈希算法: {algo}，支持: md5, sha1, sha256, sha512"
        )));
    }

    if args.len() >= 4
        && let Some(flag) = args[3].to_str()
        && (flag == "--string" || flag == "-s")
    {
        // 字符串哈希模式
        if args.len() < 5 {
            return Err(FtoolError::Input(
                "-H --string 参数需要指定要哈希的字符串\n\
                 （若想哈希一个恰好名为 '-s'/'--string' 的文件，请用 './-s' 形式指定路径）"
                    .into(),
            ));
        }
        if args.len() > 5 {
            return Err(FtoolError::Input(
                "-H --string 多余参数（字符串含空格请用引号包裹）".into(),
            ));
        }
        let data = args[4].to_string_lossy();
        let hash = Hasher::compute_string(&algo, &data)?;
        println!("{} \"{}\"", hash, data);
        return Ok(());
    }

    // 文件哈希模式
    if args.len() < 4 {
        return Err(FtoolError::Input("-H 参数需要指定算法和文件路径".into()));
    }
    if args.len() > 4 {
        return Err(FtoolError::Input(
            "-H 多余参数（文件路径含空格请用引号包裹整个路径）".into(),
        ));
    }
    let path = &args[3];
    let hash = Hasher::compute(&algo, path)?;
    println!("{} {}", hash, path.to_string_lossy());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::Sha256;
    use std::io::Cursor;

    /// SHA256("") = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
    #[test]
    fn test_hash_empty() {
        let data: &[u8] = b"";
        let result = Hasher::hash::<Sha256>(&mut Cursor::new(data)).unwrap();
        assert_eq!(
            result,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    /// SHA256("hello") = 2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824
    #[test]
    fn test_hash_hello() {
        let data = b"hello";
        let result = Hasher::hash::<Sha256>(&mut Cursor::new(data)).unwrap();
        assert_eq!(
            result,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn test_hash_md5() {
        let data = b"hello";
        let result = Hasher::hash::<md5::Md5>(&mut Cursor::new(data)).unwrap();
        assert_eq!(result, "5d41402abc4b2a76b9719d911017c592");
    }

    #[test]
    fn test_hash_sha1() {
        let data = b"hello";
        let result = Hasher::hash::<sha1::Sha1>(&mut Cursor::new(data)).unwrap();
        assert_eq!(result, "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d");
    }

    #[test]
    fn test_hash_large_buffer_tolerance() {
        // 略大于 1MB 的数据，确保分块读取不出错
        let data = vec![0xab; 1_050_000];
        let result = Hasher::hash::<Sha256>(&mut Cursor::new(data)).unwrap();
        // 不校验具体值，只确保不 panic
        assert_eq!(result.len(), 64);
    }

    // ---------- handle_command：算法白名单校验前置 ----------

    /// WHY: 算法校验必须先于文件打开——-H 拼错算法且文件路径不存在时，旧实现
    /// 先报"无法打开文件"，掩盖了真正的用法错误
    #[test]
    fn handle_command_checks_algo_before_opening_file() {
        let args: Vec<OsString> = ["ftool", "-H", "bogus-algo", "/nonexistent-ftool-hash-test"]
            .iter()
            .map(OsString::from)
            .collect();
        let err = handle_command(&args).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("不支持的哈希算法: bogus-algo"), "{msg}");
        assert!(
            !msg.contains("无法打开文件"),
            "不应被文件打开错误掩盖: {msg}"
        );
    }

    /// 白名单匹配应大小写不敏感；白名单外算法一律拒绝
    #[test]
    fn is_supported_algo_whitelist_is_case_insensitive() {
        for algo in ["md5", "SHA1", "Sha256", "sha512"] {
            assert!(is_supported_algo(algo), "{algo} 应在白名单内");
        }
        for algo in ["md4", "sha224", "sha", ""] {
            assert!(!is_supported_algo(algo), "{algo} 不应在白名单内");
        }
    }

    // ---------- EINTR 重试 ----------

    /// 首次 read 返回 Interrupted、后续正常转发的读取器
    struct InterruptOnce<R> {
        inner: R,
        interrupted: bool,
    }

    impl<R: Read> Read for InterruptOnce<R> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if !self.interrupted {
                self.interrupted = true;
                return Err(std::io::Error::from(std::io::ErrorKind::Interrupted));
            }
            self.inner.read(buf)
        }
    }

    /// WHY: read 被信号打断（EINTR）属可重试错误；若直接返回错误，一次无害的
    /// 信号就足以让大文件多次 read 中的整个哈希计算失败
    #[test]
    fn interrupted_read_is_retried_instead_of_aborting() {
        let mut reader = InterruptOnce {
            inner: Cursor::new(b"hello"),
            interrupted: false,
        };
        let result = Hasher::hash::<Sha256>(&mut reader).unwrap();
        assert_eq!(
            result,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }
}
