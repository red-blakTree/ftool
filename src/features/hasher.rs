use crate::core::FtoolError;
use digest::Digest;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{BufReader, Read};

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
            FtoolError::Input(format!("无法打开文件 '{}': {}", path.to_string_lossy(), e))
        })?;
        const BUF_SIZE: usize = 1024 * 1024; // 1MiB 缓冲区
        let mut reader = BufReader::with_capacity(BUF_SIZE, file);

        match algo {
            a if a.eq_ignore_ascii_case("md5") => Self::hash::<md5::Md5>(&mut reader),
            a if a.eq_ignore_ascii_case("sha1") => Self::hash::<sha1::Sha1>(&mut reader),
            a if a.eq_ignore_ascii_case("sha256") => Self::hash::<sha2::Sha256>(&mut reader),
            a if a.eq_ignore_ascii_case("sha512") => Self::hash::<sha2::Sha512>(&mut reader),
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
            let n = reader
                .read(&mut buffer)
                .map_err(|e| FtoolError::Input(format!("读取文件失败: {}", e)))?;
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
        Ok(String::from_utf8(hex_bytes).unwrap())
    }
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
}
