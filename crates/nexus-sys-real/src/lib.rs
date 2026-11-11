//! 驱动：`System` 的真实实现。
#![forbid(unsafe_code)]

use nexus_core_proto::Timestamp;
use nexus_sys::System;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub struct RealSystem {
    dir: PathBuf,
}

impl RealSystem {
    /// `dir` 是本节点的数据目录，不存在时创建。
    pub fn new(dir: impl AsRef<Path>) -> io::Result<Self> {
        std::fs::create_dir_all(dir.as_ref())?;
        Ok(Self { dir: dir.as_ref().to_path_buf() })
    }

    fn path(&self, key: &str) -> io::Result<PathBuf> {
        if key.is_empty() || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "非法的存储键"));
        }
        Ok(self.dir.join(format!("{key}.bin")))
    }
}

impl System for RealSystem {
    fn now(&self) -> Timestamp {
        let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
        Timestamp(d.as_millis() as u64)
    }

    fn random(&self, buf: &mut [u8]) {
        getrandom::fill(buf).expect("操作系统随机数源不可用");
    }

    fn store_get(&self, key: &str) -> io::Result<Option<Vec<u8>>> {
        match std::fs::read(self.path(key)?) {
            Ok(v) => Ok(Some(v)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn store_put(&self, key: &str, value: &[u8]) -> io::Result<()> {
        let path = self.path(key)?;
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, value)?;
        std::fs::rename(tmp, path)
    }
}
