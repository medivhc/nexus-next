//! Android 应用里的 Nexus 核心。
//!
//! 应用（`link.bewith.nexus`）的 VpnService 建好网卡后调用 `nativeStart`，把网卡的文件描述符交过来；
//! 常驻程序在一个自己的线程里运行，和命令行的 `nexus daemon` 是同一份代码（[`nexus_cli::daemon`]）。
//! 控制方式也一样：应用数据目录里的控制套接字。
//!
//! 这里有 unsafe：JNI 导出函数要求不改名（`no_mangle`），网卡来自一个原始文件描述符。

use jni::objects::{JBooleanArray, JClass, JObjectArray, JString};
use jni::sys::{jboolean, jint};
use jni::JNIEnv;
use nexus_cli::daemon;
use std::path::PathBuf;
use std::sync::Mutex;
use tokio::sync::oneshot;

/// 完整节点的网段，和应用里 VpnService 配的地址、路由、名字服务一致。
pub const TUN_NET: &str = "198.18.0.0/16";

static STOP: Mutex<Option<oneshot::Sender<()>>> = Mutex::new(None);

/// 启动常驻程序。返回 0 表示已经启动；之后的错误写到数据目录里的 daemon.log。
pub fn start(data_dir: PathBuf, name: String, tun_fd: i32, relay: String, discovery: bool) -> Result<(), String> {
    let mut stop = STOP.lock().map_err(|_| "状态损坏".to_string())?;
    if stop.is_some() {
        return Err("已经在运行".into());
    }
    std::fs::create_dir_all(&data_dir).map_err(|e| format!("无法创建数据目录：{e}"))?;
    redirect_stderr(&data_dir.join("daemon.log"));
    let pool = daemon::parse_tun_net(TUN_NET).ok_or("网段不合法")?;
    let (tx, rx) = oneshot::channel();
    *stop = Some(tx);
    std::thread::Builder::new()
        .name("nexus".into())
        .spawn(move || {
            let log = data_dir.join("daemon.log");
            let result = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string()).and_then(|rt| {
                rt.block_on(async {
                    // 网卡要在运行时里注册到事件循环。
                    // SAFETY: VpnService.establish() 给出的描述符，所有权交给我们（Java 一侧 detachFd）。
                    let device = unsafe { tun_rs::AsyncDevice::from_fd(tun_fd) }.map_err(|e| format!("网卡：{e}"))?;
                    let mut config = daemon::transport_config(false, &relay, !discovery);
                    config.scheduling = true;
                    let opts = daemon::Options {
                        data_dir: data_dir.clone(),
                        agent_socket: data_dir.join("agent.sock"),
                        name: Some(name),
                        config,
                        proxy: None,
                        tun: Some(daemon::Tun::Device { device, label: "android-vpn".into(), pool }),
                    };
                    daemon::run(opts, async {
                        let _ = rx.await;
                    })
                    .await
                    .map_err(|e| e.to_string())
                })
            });
            let line = match result {
                Ok(()) => "nexus: 已停止\n".to_string(),
                Err(e) => format!("nexus: {e}\n"),
            };
            let _ = std::fs::OpenOptions::new().create(true).append(true).open(&log).and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()));
            if let Ok(mut s) = STOP.lock() {
                *s = None;
            }
        })
        .map_err(|e| format!("无法启动线程：{e}"))?;
    Ok(())
}

/// 应用进程没有终端：核心的日志（标准错误）写到数据目录里的 daemon.log。
/// 数据目录里有 trace 文件时，各个库的调试日志也写进去（排查用）。
fn redirect_stderr(path: &std::path::Path) {
    use std::os::fd::AsRawFd;
    if let Ok(f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        // SAFETY: 把一个打开的文件复制到描述符 2；f 随后关闭不影响复制出的描述符。
        unsafe {
            libc::dup2(f.as_raw_fd(), 2);
        }
    }
    if std::env::var_os("NEXUS_TRACE").is_some() || path.with_file_name("trace").exists() {
        let _ = log::set_logger(&StderrLog).map(|()| log::set_max_level(log::LevelFilter::Trace));
    }
}

struct StderrLog;

impl log::Log for StderrLog {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }
    fn log(&self, r: &log::Record) {
        eprintln!("{} {}: {}", r.level(), r.target(), r.args());
    }
    fn flush(&self) {}
}

/// 停止常驻程序（网卡由 Java 一侧关闭）。
pub fn stop() {
    if let Ok(mut s) = STOP.lock() {
        if let Some(tx) = s.take() {
            let _ = tx.send(());
        }
    }
}

fn string(env: &mut JNIEnv, s: &JString) -> String {
    env.get_string(s).map(Into::into).unwrap_or_default()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_link_bewith_nexus_NexusVpnService_nativeStart(
    mut env: JNIEnv,
    _class: JClass,
    data_dir: JString,
    name: JString,
    tun_fd: jint,
    relay: JString,
    discovery: jboolean,
) -> jint {
    let (data_dir, name, relay) = (string(&mut env, &data_dir), string(&mut env, &name), string(&mut env, &relay));
    match start(data_dir.into(), name, tun_fd, relay, discovery != 0) {
        Ok(()) => 0,
        Err(e) => {
            let _ = env.throw_new("java/lang/IllegalStateException", e);
            -1
        }
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_link_bewith_nexus_NexusVpnService_nativeStop(_env: JNIEnv, _class: JClass) {
    stop();
}

/// 系统的网络变化时（M11.4）：应用把当前每个网络的接口名和是否计费交过来，下一次查看接口（5 秒内）时生效。
#[unsafe(no_mangle)]
pub extern "system" fn Java_link_bewith_nexus_NexusVpnService_nativeSetNetworks(
    mut env: JNIEnv,
    _class: JClass,
    names: JObjectArray,
    metered: JBooleanArray,
) {
    let n = env.get_array_length(&names).unwrap_or(0).max(0) as usize;
    let mut flags = vec![0 as jboolean; n];
    if env.get_boolean_array_region(&metered, 0, &mut flags).is_err() {
        return;
    }
    let mut list = Vec::with_capacity(n);
    for (i, m) in flags.iter().enumerate() {
        let Ok(obj) = env.get_object_array_element(&names, i as i32) else { return };
        let name = string(&mut env, &JString::from(obj));
        if !name.is_empty() {
            list.push((name, *m != 0));
        }
    }
    nexus_fabric_iroh::set_system_networks(list);
}
