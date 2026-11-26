//! 升级与回滚（M7.3，R67）。功能点见 docs/feature-points/M7.3.md。
//!
//! 常驻程序收到 `nexus upgrade --file` 后：检查新版本、换掉自己的二进制（原来的留作 `.prev`）、
//! 在数据目录的 `upgrade.json` 里记下要确认什么，然后用同样的参数执行新的二进制。新版本启动时
//! 按 `upgrade.json` 确认能连上网络，连不上就换回 `.prev`。
//!
//! Windows（M11.3，R84）：文件名是 `nexus.prev.exe`、`nexus.new.exe`；正在运行的 exe 不能被覆盖但能改名，
//! 所以换的顺序不同（[`replace`]）；重新启动是启动新进程再退出（[`reexec`]）。见 docs/feature-points/M11.3.md。

use nexus_wire::{ErrorCode, UpgradeReport, WireError};
use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// 新版本反复启动失败时，最多试几次就换回。
pub const MAX_ATTEMPTS: u32 = 3;
/// 从压缩包解出的二进制大小上限（128 MiB），防止解压炸弹耗尽内存。
const MAX_BINARY_BYTES: u64 = 128 << 20;
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
    sibling(exe, "prev")
}

/// 和二进制在同一目录的另一份：Unix 上 `nexus.<tag>`；Windows 上 `nexus.<tag>.exe`（还能被当作程序运行）。
fn sibling(exe: &Path, tag: &str) -> PathBuf {
    sibling_for(exe, tag, cfg!(windows))
}

fn sibling_for(exe: &Path, tag: &str, windows: bool) -> PathBuf {
    let file = exe.file_name().unwrap_or_default().to_string_lossy().into_owned();
    let name = match file.strip_suffix(".exe").filter(|_| windows) {
        Some(stem) => format!("{stem}.{tag}.exe"),
        None => format!("{file}.{tag}"),
    };
    exe.with_file_name(name)
}

/// 把 `src` 换到 `dst` 的位置。Windows 上正在运行的二进制不能被覆盖、但能改名：先把 `dst` 挪到 `aside`
/// （`aside` 原来有的先删掉），换不上时再挪回来。Unix 上直接改名（原子地覆盖）。
fn replace(src: &Path, dst: &Path, aside: &Path) -> io::Result<()> {
    replace_for(src, dst, aside, cfg!(windows))
}

fn replace_for(src: &Path, dst: &Path, aside: &Path, windows: bool) -> io::Result<()> {
    if !windows {
        return std::fs::rename(src, dst);
    }
    let _ = std::fs::remove_file(aside);
    let moved = dst.exists();
    if moved {
        std::fs::rename(dst, aside)?;
    }
    std::fs::rename(src, dst).inspect_err(|_| {
        if moved {
            let _ = std::fs::rename(aside, dst);
        }
    })
}

/// 给的文件：发布的 `.tar.gz`（解出里面的 `nexus`），或者二进制本身。返回放好的、可执行的新二进制
/// （和 `exe` 同一目录的 `.new`）。
fn stage(file: &Path, exe: &Path) -> Result<PathBuf, WireError> {
    let bytes = std::fs::read(file).map_err(|e| invalid(format!("读不了 {}：{e}", file.display())))?;
    verify_checksum(file, &bytes)?;
    let binary = if bytes.starts_with(&[0x1f, 0x8b]) {
        extract(&bytes)?
    } else if bytes.starts_with(b"PK\x03\x04") {
        extract_zip(&bytes)?
    } else {
        bytes
    };
    let new = sibling(exe, "new");
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
        let is_nexus = entry.path().ok().and_then(|p| p.file_name().map(is_nexus_name)).unwrap_or(false);
        if is_nexus && entry.header().entry_type().is_file() {
            let mut out = Vec::new();
            io::Read::read_to_end(&mut io::Read::take(&mut entry, MAX_BINARY_BYTES + 1), &mut out)
                .map_err(|e| invalid(format!("压缩包读不了：{e}")))?;
            if out.len() as u64 > MAX_BINARY_BYTES {
                return Err(invalid(format!("压缩包里的 nexus 超过上限（{} MiB）", MAX_BINARY_BYTES >> 20)));
            }
            return Ok(out);
        }
    }
    Err(invalid("压缩包里没有 nexus"))
}

/// 压缩包里的二进制：`nexus` 或 `nexus.exe`。
fn is_nexus_name(name: &std::ffi::OsStr) -> bool {
    name == "nexus" || name == "nexus.exe"
}

/// Windows 的发布包（`.zip`）。
fn extract_zip(bytes: &[u8]) -> Result<Vec<u8>, WireError> {
    let mut archive = zip::ZipArchive::new(io::Cursor::new(bytes)).map_err(|e| invalid(format!("压缩包读不了：{e}")))?;
    for i in 0..archive.len() {
        let mut file = archive.by_index(i).map_err(|e| invalid(format!("压缩包读不了：{e}")))?;
        let is_nexus = file.is_file() && file.enclosed_name().and_then(|p| p.file_name().map(is_nexus_name)).unwrap_or(false);
        if is_nexus {
            let mut out = Vec::new();
            io::Read::read_to_end(&mut io::Read::take(&mut file, MAX_BINARY_BYTES + 1), &mut out)
                .map_err(|e| invalid(format!("压缩包读不了：{e}")))?;
            if out.len() as u64 > MAX_BINARY_BYTES {
                return Err(invalid(format!("压缩包里的 nexus 超过上限（{} MiB）", MAX_BINARY_BYTES >> 20)));
            }
            return Ok(out);
        }
    }
    Err(invalid("压缩包里没有 nexus"))
}

/// 运行新二进制的一条命令，限时，返回标准输出。并发排空管道以避免 64 KiB 缓冲死锁。
fn probe(bin: &Path, args: &[&std::ffi::OsStr], data_dir: &Path) -> Result<String, String> {
    use io::Read;
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
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let out_t = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(r) = stdout.take() {
            let _ = r.take(1 << 20).read_to_end(&mut buf);
        }
        buf
    });
    let err_t = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(r) = stderr.take() {
            let _ = r.take(1 << 20).read_to_end(&mut buf);
        }
        buf
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if std::time::Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("20 秒内没有结束".into());
            }
        }
    };
    let out_bytes = out_t.join().unwrap_or_default();
    let err_bytes = err_t.join().unwrap_or_default();
    if !status.success() {
        return Err(format!("退出码 {}：{}", status.code().unwrap_or(-1), String::from_utf8_lossy(&err_bytes).trim()));
    }
    Ok(String::from_utf8_lossy(&out_bytes).into_owned())
}

/// 检查新版本：能运行、协议主版本相同（除非 `force`）、能读本机的数据目录。返回它的版本。
fn preflight(new: &Path, data_dir: &Path, force: bool) -> Result<String, WireError> {
    use std::ffi::OsStr;
    let v = probe(new, &[OsStr::new("--version")], data_dir).map_err(|e| invalid(format!("新版本的 nexus --version {e}")))?;
    let version = v
        .trim()
        .strip_prefix("nexus ")
        .filter(|v| !v.is_empty() && !v.contains(char::is_whitespace))
        .ok_or_else(|| invalid(format!("新版本的 nexus --version 输出的不是 nexus 的版本：{:?}", v.trim())))?
        .to_string();
    let major = probe(new, &[OsStr::new("protocol-version")], data_dir)
        .ok()
        .and_then(|p| p.split_whitespace().next().and_then(|m| m.parse::<u16>().ok()));
    if major != Some(nexus_wire::WIRE_VERSION) && !force {
        let theirs = major.map_or("不知道".to_string(), |m| m.to_string());
        return Err(invalid(format!(
            "新版本的协议主版本是 {theirs}，本机是 {}：换上之后连不上没升级的设备。确定要换就加 --force",
            nexus_wire::WIRE_VERSION
        )));
    }
    probe(
        new,
        &[OsStr::new("--data-dir"), data_dir.as_os_str(), OsStr::new("daemon"), OsStr::new("--preflight")],
        data_dir,
    )
    .map_err(|e| invalid(format!("新版本读不了本机的数据目录：{e}")))?;
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
    // 上一次在 Windows 上换回时留下的（当时还在运行，删不掉）。
    let _ = std::fs::remove_file(sibling(exe, "old"));
    let swap = || -> io::Result<()> {
        if cfg!(windows) {
            // 正在运行的 exe 改名为 .prev，新版本改名到它的位置。
            return replace(&new, exe, &prev);
        }
        // 原来的留一份：hard link 失败（例如跨文件系统）就复制。
        let _ = std::fs::remove_file(&prev);
        if std::fs::hard_link(exe, &prev).is_err() {
            std::fs::copy(exe, &prev)?;
        }
        std::fs::rename(&new, exe)
    };
    swap().map_err(|e| {
        let _ = std::fs::remove_file(&new);
        WireError::new(ErrorCode::InvalidState, format!("换不了 {}：{e}", exe.display()))
    })?;
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
    if let Err(e) = save(data_dir, &state) {
        // 状态文件写失败时立刻换回旧二进制，避免没有 upgrade.json 导致无法自动回滚。
        let _ = std::fs::rename(&state.prev, &state.exe).or_else(|_| std::fs::copy(&state.prev, &state.exe).map(|_| ()));
        return Err(WireError::new(ErrorCode::Internal, format!("写不了 upgrade.json：{e}")));
    }
    Ok(state)
}

/// 换回 `.prev`。Windows 上正在运行的（新版本）先改名为 `nexus.old.exe`。
pub fn restore(state: &mut UpgradeState, data_dir: &Path, why: &str) -> io::Result<()> {
    if cfg!(windows) {
        replace(&state.prev, &state.exe, &sibling(&state.exe, "old"))?;
    } else {
        std::fs::rename(&state.prev, &state.exe).or_else(|_| std::fs::copy(&state.prev, &state.exe).map(|_| ()))?;
    }
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

/// Windows 不能原地换进程：用同样的参数启动新的（继承标准输入输出），然后本进程退出。进程号会变。
/// 新进程的控制入口在旧的退出前就开始监听：命名管道允许多个实例，旧的退出后只剩新的。
#[cfg(windows)]
pub fn reexec(exe: &Path) -> io::Error {
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    match std::process::Command::new(exe).args(args).env_remove("NEXUS_TEST_NO_NETWORK").spawn() {
        Ok(_) => std::process::exit(0),
        Err(e) => e,
    }
}

#[cfg(not(any(unix, windows)))]
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

    fn temp_dir(tag: &str) -> Dir {
        let path = std::env::temp_dir().join(format!("nexus-upgrade-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Dir(path)
    }

    #[test]
    fn prev_is_next_to_the_binary() {
        let unix = Path::new("/usr/local/bin/nexus");
        assert_eq!(sibling_for(unix, "prev", false), Path::new("/usr/local/bin/nexus.prev"));
        assert_eq!(sibling_for(unix, "new", false), Path::new("/usr/local/bin/nexus.new"));
        // covers: R84
        // Windows：还是 .exe，能被当作程序运行。没有 .exe 后缀的照 Unix 的规则。
        let win = Path::new("/apps/nexus/nexus.exe");
        assert_eq!(sibling_for(win, "prev", true), Path::new("/apps/nexus/nexus.prev.exe"));
        assert_eq!(sibling_for(win, "new", true), Path::new("/apps/nexus/nexus.new.exe"));
        assert_eq!(sibling_for(win, "old", true), Path::new("/apps/nexus/nexus.old.exe"));
        assert_eq!(sibling_for(unix, "prev", true), Path::new("/usr/local/bin/nexus.prev"));
        assert_eq!(prev_path(Path::new("nexus")), Path::new("nexus.prev"));
    }

    /// Windows 的换法：先把原来的挪开再放新的；换回时正在运行的那份挪到 `.old`。在所有平台上按这个顺序走一遍。
    #[test]
    fn windows_swap_order_keeps_every_version() {
        let dir = temp_dir("swap");
        let exe = dir.path().join("nexus.exe");
        let (new, prev, old) = (sibling_for(&exe, "new", true), sibling_for(&exe, "prev", true), sibling_for(&exe, "old", true));
        std::fs::write(&exe, b"v1").unwrap();
        std::fs::write(&prev, b"stale").unwrap();
        std::fs::write(&new, b"v2").unwrap();
        // 换上：原来的成了 .prev（旧的 .prev 被替换），新的在原位，.new 不在了。
        replace_for(&new, &exe, &prev, true).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"v2");
        assert_eq!(std::fs::read(&prev).unwrap(), b"v1");
        assert!(!new.exists());
        // 换回：正在运行的 v2 挪到 .old，v1 回到原位。
        replace_for(&prev, &exe, &old, true).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"v1");
        assert_eq!(std::fs::read(&old).unwrap(), b"v2");
        assert!(!prev.exists());
        // 换不上（源文件不在）：原来的挪回原位。
        replace_for(&dir.path().join("missing"), &exe, &old, true).unwrap_err();
        assert_eq!(std::fs::read(&exe).unwrap(), b"v1");
    }

    #[test]
    fn binary_is_found_in_zip_and_tar_gz() {
        use std::io::Write;
        // .zip（Windows 的发布包）：目录里的 nexus.exe。
        let mut buf = io::Cursor::new(Vec::new());
        {
            let mut z = zip::ZipWriter::new(&mut buf);
            let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
            z.start_file("nexus-0.2.0-windows-x86_64/README.md", opts).unwrap();
            z.write_all(b"readme").unwrap();
            z.start_file("nexus-0.2.0-windows-x86_64/nexus.exe", opts).unwrap();
            z.write_all(b"MZ binary").unwrap();
            z.finish().unwrap();
        }
        let bytes = buf.into_inner();
        assert!(bytes.starts_with(b"PK\x03\x04"));
        assert_eq!(extract_zip(&bytes).unwrap(), b"MZ binary");
        // 没有 nexus 的压缩包
        let mut buf = io::Cursor::new(Vec::new());
        {
            let mut z = zip::ZipWriter::new(&mut buf);
            z.start_file("other.txt", zip::write::SimpleFileOptions::default()).unwrap();
            z.write_all(b"x").unwrap();
            z.finish().unwrap();
        }
        assert!(extract_zip(&buf.into_inner()).is_err());
        // .tar.gz：nexus.exe 也认。
        let mut tar_bytes = Vec::new();
        {
            let gz = flate2::write::GzEncoder::new(&mut tar_bytes, flate2::Compression::fast());
            let mut t = tar::Builder::new(gz);
            let mut h = tar::Header::new_gnu();
            h.set_size(3);
            h.set_mode(0o755);
            h.set_cksum();
            t.append_data(&mut h, "d/nexus.exe", &b"bin"[..]).unwrap();
            t.into_inner().unwrap().finish().unwrap();
        }
        assert_eq!(extract(&tar_bytes).unwrap(), b"bin");
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
