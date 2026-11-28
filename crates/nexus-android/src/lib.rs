//! Android 应用里的 Nexus 核心。
//!
//! 应用（`link.bewith.nexus`）的 VpnService 建好网卡后调用 `nativeStart`，把网卡的文件描述符交过来；
//! 常驻程序在一个自己的线程里运行，和命令行的 `nexus daemon` 是同一份代码（[`nexus_cli::daemon`]）。
//! 控制方式也一样：应用数据目录里的控制套接字。
//!
//! 这里有 unsafe：JNI 导出函数要求不改名（`no_mangle`），网卡来自一个原始文件描述符。

use jni::objects::{JBooleanArray, JClass, JObject, JObjectArray, JString, JValue};
use jni::sys::{jboolean, jint, jlong, jstring};
use jni::JNIEnv;
use nexus_cli::daemon;
use nexus_cli::tun::{AppRouter, TrafficObserver, TunRouting};
use nexus_net_core::{route::Outbound, Target};
use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

/// 完整节点的网段，和应用里 VpnService 配的地址、路由、名字服务一致。
pub const TUN_NET: &str = "198.18.0.0/16";

struct DaemonStop {
    signal: Option<oneshot::Sender<()>>,
}

static STOP: Mutex<Option<DaemonStop>> = Mutex::new(None);

/// 启动常驻程序。返回 0 表示已经启动；之后的错误写到数据目录里的 daemon.log。
/// `rendezvous` 和 `nexus daemon --rendezvous` 一样：空串或 `default` 跟随 `discovery`，`disabled` 不用，其余是地址
/// （自建网络里用锚点的 `https://<锚点>/pkarr`）。
fn start(data_dir: PathBuf, name: String, tun_fd: OwnedFd, relay: String, discovery: bool, rendezvous: String,
    (app_router, on_traffic): (Option<AppRouter>, Option<TrafficObserver>)) -> Result<(), String> {
    let mut stop = STOP.lock().map_err(|_| "状态损坏".to_string())?;
    if stop.is_some() {
        return Err("已经在运行".into());
    }
    std::fs::create_dir_all(&data_dir).map_err(|e| format!("无法创建数据目录：{e}"))?;
    redirect_stderr(&data_dir.join("daemon.log"));
    let pool = daemon::parse_tun_net(TUN_NET).ok_or("网段不合法")?;
    let (tx, rx) = oneshot::channel();
    *stop = Some(DaemonStop { signal: Some(tx) });
    if let Err(e) = std::thread::Builder::new()
        .name("nexus".into())
        .spawn(move || {
            let log = data_dir.join("daemon.log");
            // ipstack closes active TCP flows with block_in_place.
            let result = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().map_err(|e| e.to_string()).and_then(|rt| {
                rt.block_on(async {
                    // 网卡要在运行时里注册到事件循环。
                    // SAFETY: VpnService.establish() 给出的描述符，所有权交给我们（Java 一侧 detachFd）。
                    let device = unsafe { tun_rs::AsyncDevice::from_fd(tun_fd.into_raw_fd()) }.map_err(|e| format!("网卡：{e}"))?;
                    let mut config = daemon::transport_config(false, &relay, !discovery);
                    if let Some(r) = rendezvous_override(&rendezvous) {
                        config.rendezvous = r;
                    }
                    config.scheduling = true;
                    let opts = daemon::Options {
                        data_dir: data_dir.clone(),
                        agent_socket: data_dir.join("agent.sock"),
                        name: Some(name),
                        config,
                        proxy: None,
                        tun: Some(daemon::Tun::Device { device, label: "android-vpn".into(), pool, app_router, on_traffic }),
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
    {
        *stop = None;
        return Err(format!("无法启动线程：{e}"));
    }
    Ok(())
}

/// 应用启动参数里的碰头服务（M11.2）：`None` 表示沿用默认（跟随 discovery）。
pub fn rendezvous_override(text: &str) -> Option<Option<String>> {
    match text.trim() {
        "" | "default" => None,
        "disabled" => Some(None),
        url => Some(Some(url.to_string())),
    }
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

/// 停止常驻程序，退出运行时后释放 VPN 网卡。
pub fn stop() {
    if let Ok(mut s) = STOP.lock() {
        if let Some(tx) = s.as_mut().and_then(|daemon| daemon.signal.take()) {
            let _ = tx.send(());
        }
    }
}

/// 应用界面刷新一次（M11.2，R79）：经数据目录里的控制套接字问核心，返回 [`nexus_cli::app::Screen`] 的 JSON。
/// `held` 是上次返回的加入码（`Screen::Joining` 的 JSON，没有时为空串），`renew` 要求换一个加入码。
pub fn screen_json(data_dir: &std::path::Path, held: &str, renew: bool, now_ms: u64) -> String {
    use nexus_cli::app::{refresh, Screen};
    let held = match serde_json::from_str::<Screen>(held) {
        Ok(Screen::Joining(view)) => Some(view),
        _ => None,
    };
    let ctl = nexus_cli::UnixCtl::new(data_dir, None);
    let screen = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt.block_on(async {
            // 核心卡住时界面不能跟着卡住。
            tokio::time::timeout(std::time::Duration::from_secs(15), refresh(&ctl, held, renew, now_ms))
                .await
                .unwrap_or(Screen::Failed { message: "核心没有回应".into() })
        }),
        Err(e) => Screen::Failed { message: e.to_string() },
    };
    serde_json::to_string(&screen).unwrap_or_default()
}

fn route_command(data_dir: &std::path::Path, operation: &str, domain: &str, endpoint: &str) -> Result<String, String> {
    use nexus_cli::{Captured, Cmd, Output, RouteCmd, UnixCtl};
    use nexus_net_core::route::Outbound;
    let ctl = UnixCtl::new(data_dir, None);
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
    let timeout = std::time::Duration::from_secs(10);
    if let Some(mode) = operation.strip_prefix("import_") {
        if endpoint.is_empty() {
            return Err("请填写导入规则使用的出口".into());
        }
        let selected = match mode {
            "exit" => Outbound::Exit(endpoint.to_string()),
            "socks5" => Outbound::Socks5(endpoint.to_string()),
            _ => return Err("未知的导入出口类型".into()),
        };
        return runtime.block_on(async {
            let count = tokio::time::timeout(timeout, nexus_cli::app::import_rules(&ctl, domain, selected))
                .await.map_err(|_| "核心没有回应".to_string())??;
            Ok(format!("已追加 {count} 条规则。已有规则仍优先匹配。"))
        });
    }
    let cmd = match operation {
        "list" => RouteCmd::Ls { json: false },
        "remove" => {
            let index = domain.parse::<u32>().ok().filter(|&n| n != 0).ok_or("请输入要删除的规则编号")?;
            RouteCmd::Rm { index, json: false }
        }
        "exit" | "socks5" | "reject" => {
            if operation != "reject" && endpoint.is_empty() {
                return Err("请填写出口设备名或 SOCKS5 地址".into());
            }
            RouteCmd::Add {
                domain: Some(domain.to_string()), cidr: None, reject: operation == "reject", direct: false,
                via: (operation == "exit").then(|| endpoint.to_string()),
                socks5: (operation == "socks5").then(|| endpoint.to_string()), json: false,
            }
        }
        _ => return Err("未知的规则操作".into()),
    };
    runtime.block_on(async {
        let mut captured = Captured::default();
        let request = Cmd::Route { cmd };
        let mut input = tokio::io::empty();
        let code = tokio::time::timeout(
            timeout,
            nexus_cli::run_client(&request, &ctl, &mut input, &mut captured),
        ).await.map_err(|_| "核心没有回应".to_string())?;
        let output = Output::from_captured(code, captured);
        if code == 0 {
            return Ok(output.stdout);
        }
        let message = serde_json::from_str::<serde_json::Value>(&output.stderr).ok()
            .and_then(|value| value.get("message")?.as_str().map(str::to_string)).unwrap_or(output.stderr);
        Err(message)
    })
}

fn string(env: &mut JNIEnv, s: &JString) -> String {
    env.get_string(s).map(Into::into).unwrap_or_default()
}

fn app_router(env: &mut JNIEnv, service: &JObject) -> jni::errors::Result<AppRouter> {
    let vm = env.get_java_vm()?;
    let service = env.new_global_ref(service)?;
    Ok(Arc::new(move |protocol, src, dst| {
        let mut env = vm.attach_current_thread().ok()?;
        let result = env.with_local_frame(8, |env| -> jni::errors::Result<Option<TunRouting>> {
            let src_ip = env.new_string(src.ip().to_string())?;
            let dst_ip = env.new_string(dst.ip().to_string())?;
            let result = env.call_method(
                service.as_obj(), "routeForConnection", "(ILjava/lang/String;ILjava/lang/String;I)Ljava/lang/String;",
                &[
                    JValue::Int(protocol.into()), JValue::Object(src_ip.as_ref()), JValue::Int(src.port().into()),
                    JValue::Object(dst_ip.as_ref()), JValue::Int(dst.port().into()),
                ],
            )?.l()?;
            if result.is_null() {
                return Ok(None);
            }
            let route_string = JString::from(result);
            let route: String = env.get_string(&route_string)?.into();
            let outbound = if route.is_empty() {
                None
            } else if let Some(device) = route.strip_prefix("exit:").filter(|s| !s.is_empty()) {
                Some(Outbound::Exit(device.into()))
            } else if let Some(address) = route.strip_prefix("socks5:").filter(|s| Target::parse(s).is_some_and(|target| target.port != 0)) {
                Some(Outbound::Socks5(address.into()))
            } else {
                return Ok(None);
            };
            Ok(Some(TunRouting::Application { outbound }))
        });
        if result.is_err() && env.exception_check().unwrap_or(false) {
            let _ = env.exception_clear();
        }
        result.ok().flatten()
    }))
}

fn traffic_observer(env: &mut JNIEnv, service: &JObject) -> jni::errors::Result<TrafficObserver> {
    let vm = env.get_java_vm()?;
    let service = env.new_global_ref(service)?;
    Ok(Box::new(move || {
        if let Ok(mut env) = vm.attach_current_thread() {
            if env.call_method(service.as_obj(), "onTunTraffic", "()V", &[]).is_err()
                && env.exception_check().unwrap_or(false)
            {
                let _ = env.exception_clear();
            }
        }
    }))
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_link_bewith_nexus_NexusVpnService_nativeStart(
    mut env: JNIEnv,
    _this: JObject,
    data_dir: JString,
    name: JString,
    tun_fd: jint,
    relay: JString,
    discovery: jboolean,
    rendezvous: JString,
    full_capture: jboolean,
    adaptive_cellular: jboolean,
) -> jint {
    #[cfg(target_os = "android")]
    {
        static INIT_CTX: std::sync::Once = std::sync::Once::new();
        INIT_CTX.call_once(|| {
            let ctx = env
                .call_method(&_this, "getApplicationContext", "()Landroid/content/Context;", &[])
                .ok()
                .and_then(|v| v.l().ok())
                .unwrap_or_else(|| env.new_local_ref(&_this).unwrap());
            if let (Ok(vm), Ok(global)) = (env.get_java_vm(), env.new_global_ref(ctx)) {
                let vm_ptr = vm.get_java_vm_pointer().cast();
                let ctx_ptr = global.as_raw().cast();
                std::mem::forget(global);
                // SAFETY: Android 上只初始化一次，vm 与 Application Context 的全局引用在进程生命周期内有效。
                unsafe { ndk_context::initialize_android_context(vm_ptr, ctx_ptr) };
            }
        });
    }
    let (data_dir, name, relay) = (string(&mut env, &data_dir), string(&mut env, &name), string(&mut env, &relay));
    let rendezvous = string(&mut env, &rendezvous);
    // SAFETY: Java passes ownership of its open VPN descriptor via detachFd().
    let tun_fd = unsafe { OwnedFd::from_raw_fd(tun_fd) };
    let observers = (|| -> jni::errors::Result<_> {
        let router = (full_capture != 0).then(|| app_router(&mut env, &_this)).transpose()?;
        let traffic = (adaptive_cellular != 0).then(|| traffic_observer(&mut env, &_this)).transpose()?;
        Ok((router, traffic))
    })();
    let observers = match observers {
        Ok(observers) => observers,
        Err(e) => {
            let _ = env.throw_new("java/lang/IllegalStateException", e.to_string());
            return -1;
        }
    };
    match start(data_dir.into(), name, tun_fd, relay, discovery != 0, rendezvous, observers) {
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

#[unsafe(no_mangle)]
pub extern "system" fn Java_link_bewith_nexus_NexusVpnService_nativeStopping(_env: JNIEnv, _class: JClass) -> jboolean {
    STOP.lock().is_ok_and(|s| s.as_ref().is_some_and(|daemon| daemon.signal.is_none())) as jboolean
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

#[unsafe(no_mangle)]
pub extern "system" fn Java_link_bewith_nexus_NexusCore_nativeScreen(
    mut env: JNIEnv,
    _class: JClass,
    data_dir: JString,
    held: JString,
    renew: jboolean,
    now_ms: jlong,
) -> jstring {
    let (data_dir, held) = (string(&mut env, &data_dir), string(&mut env, &held));
    let json = screen_json(std::path::Path::new(&data_dir), &held, renew != 0, now_ms.max(0) as u64);
    env.new_string(json).map(|s| s.into_raw()).unwrap_or(std::ptr::null_mut())
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_link_bewith_nexus_NexusCore_nativeRoutes(
    mut env: JNIEnv, _class: JClass, data_dir: JString, operation: JString, domain: JString, endpoint: JString,
) -> jstring {
    let data_dir = string(&mut env, &data_dir);
    let operation = string(&mut env, &operation);
    let domain = string(&mut env, &domain);
    let endpoint = string(&mut env, &endpoint);
    let message = route_command(std::path::Path::new(&data_dir), &operation, domain.trim(), endpoint.trim())
        .unwrap_or_else(|e| format!("规则操作失败：{e}"));
    env.new_string(message).map(|s| s.into_raw()).unwrap_or(std::ptr::null_mut())
}

#[cfg(test)]
mod tests {
    // covers: R79
    use super::*;

    #[test]
    fn rendezvous_extra() {
        assert_eq!(rendezvous_override(""), None);
        assert_eq!(rendezvous_override("default"), None);
        assert_eq!(rendezvous_override("disabled"), Some(None));
        assert_eq!(rendezvous_override("http://10.0.0.1:8080/pkarr"), Some(Some("http://10.0.0.1:8080/pkarr".into())));
    }

    // FP-11.2-06：核心还没起来（没有控制套接字）时界面显示正在启动；交回的内容不对也不影响
    #[test]
    fn screen_before_the_core_is_up() {
        let dir = std::env::temp_dir().join(format!("nexus-android-test-{}", std::process::id()));
        for held in ["", "{", r#"{"state":"starting","message":"x"}"#] {
            let v: serde_json::Value = serde_json::from_str(&screen_json(&dir, held, false, 0)).unwrap();
            assert_eq!(v["state"], "starting", "{v}");
        }
    }
}
