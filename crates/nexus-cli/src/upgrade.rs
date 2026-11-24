//! 升级与回滚（M7.3，R67）。功能点见 docs/feature-points/M7.3.md。
//!
//! 常驻程序收到 `nexus upgrade --file` 后：检查新版本、换掉自己的二进制（原来的留作 `.prev`）、
//! 在数据目录的 `upgrade.json` 里记下要确认什么，然后用同样的参数执行新的二进制。新版本启动时
//! 按 `upgrade.json` 确认能连上网络，连不上就换回 `.prev`。

use nexus_wire::{ErrorCode, UpgradeReport, WireError};
use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// 新版本反复启动失败时，最多试几次就换回。
pub const MAX_ATTEMPTS: u32 = 3;
const STATE_FILE: &str = "upgrade.json";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpgradeState {
    /// pending、confirmed、rolled_back
    pub state: String,
    pub from: String,
    pub to: String,
    /// 被换掉的二进制，和原来的那份。
    pub exe: PathBuf,
    pub prev: PathBuf,
    /// 升级前在线的设备：新版本要连上其中一台才算成功。
    pub peers: Vec<String>,
    pub confirm_ms: u64,
    pub attempts: u32,
    pub message: String,
    /// 换上时的毫秒时间，用作这次升级的编号。
    #[serde(default)]
    pub id: u64,
}

impl UpgradeState {
    pub fn report(&self) -> UpgradeReport {
        UpgradeReport {
            state: self.state.clone(),
            from: self.from.clone(),
            to: self.to.clone(),
            exe: self.exe.display().to_string(),
            message: self.message.clone(),
            id: self.id,
        }
    }
}

fn invalid(msg: impl Into<String>) -> WireError {
    WireError::new(ErrorCode::InvalidInput, msg)
}

fn state_path(data_dir: &Path) -> PathBuf {
    data_dir.join(STATE_FILE)
}

pub fn load(data_dir: &Path) -> Option<UpgradeState> {
    serde_json::from_slice(&std::fs::read(state_path(data_dir)).ok()?).ok()
}

pub fn save(data_dir: &Path, s: &UpgradeState) -> io::Result<()> {
    let tmp = data_dir.join(format!("{STATE_FILE}.tmp"));
    std::fs::write(&tmp, serde_json::to_vec_pretty(s).expect("总能编码"))?;
    std::fs::rename(tmp, state_path(data_dir))
}

/// `.prev`：和二进制在同一目录（改名是原子的）。
pub fn prev_path(exe: &Path) -> PathBuf {
    let mut name = exe.file_name().unwrap_or_default().to_os_string();
    name.push(".prev");
    exe.with_file_name(name)
}

fn sibling(exe: &Path, suffix: &str) -> PathBuf {
    let mut name = exe.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    exe.with_file_name(name)
}

/// 给的文件：发布的 `.tar.gz`（解出里面的 `nexus`），或者二进制本身。返回放好的、可执行的新二进制
/// （和 `exe` 同一目录的 `.new`）。
fn stage(file: &Path, exe: &Path) -> Result<PathBuf, WireError> {
    let bytes = std::fs::read(file).map_err(|e| invalid(format!("读不了 {}：{e}", file.display())))?;
    verify_checksum(file, &bytes)?;
    let binary = if bytes.starts_with(&[0x1f, 0x8b]) { extract(&bytes)? } else { bytes };
    let new = sibling(exe, ".new");
    let write = || -> io::Result<()> {
        std::fs::write(&new, &binary)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&new, std::fs::Permissions::from_mode(0o755))?;
        }
        Ok(())
    };
    write().map_err(|e| {
        WireError::new(
            ErrorCode::InvalidState,
            format!("没法在 {} 旁边写新版本（{e}）：常驻程序要能写它所在的目录，比如用 sudo 运行，或者把 nexus 装在自己的目录里", exe.display()),
        )
    })?;
    Ok(new)
}

/// 压缩包旁边有 `SHA256SUMS`（发布流程生成的）就校验。
pub fn verify_checksum(file: &Path, bytes: &[u8]) -> Result<(), WireError> {
    let Some(sums) = file.parent().map(|d| d.join("SHA256SUMS")) else { return Ok(()) };
    let Ok(text) = std::fs::read_to_string(&sums) else { return Ok(()) };
    let name = file.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    let Some(want) = text.lines().find_map(|l| {
        let (hash, n) = l.split_once(char::is_whitespace)?;
        (n.trim().trim_start_matches('*') == name).then(|| hash.to_ascii_lowercase())
    }) else {
        return Ok(());
    };
    use sha2::Digest;
    let got: String = sha2::Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect();
    if got != want {
        return Err(invalid(format!("{name} 的 SHA256 和 SHA256SUMS 里的不一样：文件可能下载不完整或被改过")));
    }
    Ok(())
}

fn extract(gz: &[u8]) -> Result<Vec<u8>, WireError> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(gz));
    let entries = archive.entries().map_err(|e| invalid(format!("压缩包读不了：{e}")))?;
    for entry in entries {
        let mut entry = entry.map_err(|e| invalid(format!("压缩包读不了：{e}")))?;
        let is_nexus = entry.path().ok().and_then(|p| p.file_name().map(|n| n == "nexus")).unwrap_or(false);
        if is_nexus && entry.header().entry_type().is_file() {
            let mut out = Vec::new();
            io::Read::read_to_end(&mut entry, &mut out).map_err(|e| invalid(format!("压缩包读不了：{e}")))?;
            return Ok(out);
        }
    }
    Err(invalid("压缩包里没有 nexus"))
}

/// 运行新二进制的一条命令，限时，返回标准输出。
fn probe(bin: &Path, args: &[&str], data_dir: &Path) -> Result<String, String> {
    let mut cmd = std::process::Command::new(bin);
    cmd.args(args)
        .env("NEXUS_DATA_DIR", data_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    // 刚写好的文件：别的线程同时 fork 出的子进程在 exec 之前还持有它的写句柄时，执行会得到 ETXTBSY，等一下再试。
    let mut tries = 0;
    let mut child = loop {
        match cmd.spawn() {
            Err(e) if e.kind() == io::ErrorKind::ExecutableFileBusy && tries < 100 => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(20));
            }
            other => break other.map_err(|e| format!("运行不了（{e}）"))?,
        }
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                return Err("20 秒内没有结束".into());
            }
        }
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!("退出码 {}：{}", out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// 检查新版本：能运行、协议主版本相同（除非 `force`）、能读本机的数据目录。返回它的版本。
fn preflight(new: &Path, data_dir: &Path, force: bool) -> Result<String, WireError> {
    let v = probe(new, &["--version"], data_dir).map_err(|e| invalid(format!("新版本的 nexus --version {e}")))?;
    let version = v
        .trim()
        .strip_prefix("nexus ")
        .filter(|v| !v.is_empty() && !v.contains(char::is_whitespace))
        .ok_or_else(|| invalid(format!("新版本的 nexus --version 输出的不是 nexus 的版本：{:?}", v.trim())))?
        .to_string();
    let major = probe(new, &["protocol-version"], data_dir)
        .ok()
        .and_then(|p| p.split_whitespace().next().and_then(|m| m.parse::<u16>().ok()));
    if major != Some(nexus_wire::WIRE_VERSION) && !force {
        let theirs = major.map_or("不知道".to_string(), |m| m.to_string());
        return Err(invalid(format!(
            "新版本的协议主版本是 {theirs}，本机是 {}：换上之后连不上没升级的设备。确定要换就加 --force",
            nexus_wire::WIRE_VERSION
        )));
    }
    let dir = data_dir.to_str().unwrap_or_default();
    probe(new, &["--data-dir", dir, "daemon", "--preflight"], data_dir).map_err(|e| invalid(format!("新版本读不了本机的数据目录：{e}")))?;
    Ok(version)
}

/// 换上新版本，记下 `upgrade.json`。之后由调用方用同样的参数执行 `exe`。
pub fn install(file: &Path, exe: &Path, data_dir: &Path, peers: Vec<String>, confirm_ms: u64, force: bool) -> Result<UpgradeState, WireError> {
    let new = stage(file, exe)?;
    let version = match preflight(&new, data_dir, force) {
        Ok(v) => v,
        Err(e) => {
            let _ = std::fs::remove_file(&new);
            return Err(e);
        }
    };
    let prev = prev_path(exe);
    let swap = || -> io::Result<()> {
        // 原来的留一份：hard link 失败（例如跨文件系统）就复制。
        let _ = std::fs::remove_file(&prev);
        if std::fs::hard_link(exe, &prev).is_err() {
            std::fs::copy(exe, &prev)?;
        }
        std::fs::rename(&new, exe)
    };
    swap().map_err(|e| WireError::new(ErrorCode::InvalidState, format!("换不了 {}：{e}", exe.display())))?;
    let state = UpgradeState {
        state: "pending".into(),
        from: env!("CARGO_PKG_VERSION").into(),
        to: version,
        exe: exe.to_path_buf(),
        prev,
        peers,
        confirm_ms,
        attempts: 0,
        message: String::new(),
        id: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(1, |d| d.as_millis() as u64),
    };
    save(data_dir, &state).map_err(|e| WireError::new(ErrorCode::Internal, format!("写不了 upgrade.json：{e}")))?;
    Ok(state)
}

/// 换回 `.prev`。
pub fn restore(state: &mut UpgradeState, data_dir: &Path, why: &str) -> io::Result<()> {
    std::fs::rename(&state.prev, &state.exe).or_else(|_| std::fs::copy(&state.prev, &state.exe).map(|_| ()))?;
    state.state = "rolled_back".into();
    state.message = why.to_string();
    save(data_dir, state)
}

/// 用和现在一样的参数执行 `exe`（进程号不变）。只在出错时返回。
#[cfg(unix)]
pub fn reexec(exe: &Path) -> io::Error {
    use std::os::unix::process::CommandExt;
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    // 测试用的环境变量不带给下一个版本。
    std::process::Command::new(exe).args(args).env_remove("NEXUS_TEST_NO_NETWORK").exec()
}

#[cfg(not(unix))]
pub fn reexec(_exe: &Path) -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, "这个系统上不支持自动重新启动")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用的临时目录，结束时删掉。
    struct Dir(PathBuf);
    impl Dir {
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn prev_is_next_to_the_binary() {
        assert_eq!(prev_path(Path::new("/usr/local/bin/nexus")), Path::new("/usr/local/bin/nexus.prev"));
    }

    #[test]
    fn checksum_mismatch_is_refused() {
        let path = std::env::temp_dir().join(format!("nexus-upgrade-test-{}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        let dir = Dir(path);
        let file = dir.path().join("nexus-0.2.0-linux-x86_64.tar.gz");
        std::fs::write(&file, b"abc").unwrap();
        // sha256("abc")
        let good = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        std::fs::write(dir.path().join("SHA256SUMS"), format!("{good}  nexus-0.2.0-linux-x86_64.tar.gz\n")).unwrap();
        assert!(verify_checksum(&file, b"abc").is_ok());
        assert!(verify_checksum(&file, b"abd").is_err());
        // 不在 SHA256SUMS 里的文件：不校验
        assert!(verify_checksum(&dir.path().join("other"), b"x").is_ok());
    }
}
