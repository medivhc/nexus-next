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
fn start(
    data_dir: PathBuf,
    name: String,
    tun_fd: OwnedFd,
    relay: String,
    discovery: bool,
    rendezvous: String,
    (app_router, on_traffic): (Option<AppRouter>, Option<TrafficObserver>),
) -> Result<(), String> {
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
            let result = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .map_err(|e| e.to_string())
                .and_then(|rt| {
                    rt.block_on(async {
                        // 网卡要在运行时里注册到事件循环。
                        // SAFETY: VpnService.establish() 给出的描述符，所有权交给我们（Java 一侧 detachFd）。
                        let device = unsafe { tun_rs::AsyncDevice::from_fd(tun_fd.into_raw_fd()) }
                            .map_err(|e| format!("网卡：{e}"))?;
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
                            tun: Some(daemon::Tun::Device {
                                device,
                                label: "android-vpn".into(),
                                pool,
                                app_router,
                                on_traffic,
                            }),
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
            let result = env
                .call_method(
                    service.as_obj(),
                    "routeForConnection",
                    "(ILjava/lang/String;ILjava/lang/String;I)Ljava/lang/String;",
                    &[
                        JValue::Int(protocol.into()),
                        JValue::Object(src_ip.as_ref()),
                        JValue::Int(src.port().into()),
                        JValue::Object(dst_ip.as_ref()),
                        JValue::Int(dst.port().into()),
                    ],
                )?
                .l()?;
            if result.is_null() {
                return Ok(None);
            }
            let route_string = JString::from(result);
            let route: String = env.get_string(&route_string)?.into();
            let outbound = if route.is_empty() {
                None
            } else if let Some(device) = route.strip_prefix("exit:").filter(|s| !s.is_empty()) {
                Some(Outbound::Exit(device.into()))
            } else if let Some(address) = route
                .strip_prefix("socks5:")
                .filter(|s| Target::parse(s).is_some_and(|target| target.port != 0))
            {
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
            if env
                .call_method(service.as_obj(), "onTunTraffic", "()V", &[])
                .is_err()
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
        let router = (full_capture != 0)
            .then(|| app_router(&mut env, &_this))
            .transpose()?;
        let traffic = (adaptive_cellular != 0)
            .then(|| traffic_observer(&mut env, &_this))
            .transpose()?;
        Ok((router, traffic))
    })();
    let observers = match observers {
        Ok(observers) => observers,
        Err(e) => {
            let _ = env.throw_new("java/lang/IllegalStateException", e.to_string());
            return -1;
        }
    };
    match start(
        data_dir.into(),
        name,
        tun_fd,
        relay,
        discovery != 0,
        rendezvous,
        observers,
    ) {
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
pub extern "system" fn Java_link_bewith_nexus_NexusVpnService_nativeStopping(
    _env: JNIEnv,
    _class: JClass,
) -> jboolean {
    STOP.lock()
        .is_ok_and(|s| s.as_ref().is_some_and(|daemon| daemon.signal.is_none())) as jboolean
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


/// N2 电源模式查询：判断当前模式在有 Wi-Fi 时是否需要主动请求蜂窝网络。
#[unsafe(no_mangle)]
pub extern "system" fn Java_link_bewith_nexus_NexusCore_nativeShouldKeepCellular(
    mut env: JNIEnv,
    _class: JClass,
    power_mode: JString,
) -> jboolean {
    let mode_str = string(&mut env, &power_mode);
    let mode = nexus_net_core::power::PowerMode::parse(&mode_str);
    jboolean::from(nexus_net_core::power::PowerPolicy::for_mode(mode).keep_cellular_with_wifi)
}

/// N1 应用绕过列表归一化：返回按换行分隔的最终 `disallowed_packages`（强制包含自身包名防 VPN 环路）。
#[unsafe(no_mangle)]
pub extern "system" fn Java_link_bewith_nexus_NexusCore_nativeEffectiveBypass(
    mut env: JNIEnv,
    _class: JClass,
    self_pkg: JString,
    raw_lines: JString,
) -> jstring {
    let self_pkg = string(&mut env, &self_pkg);
    let raw = string(&mut env, &raw_lines);
    let policy = nexus_net_core::app_policy::AppPolicy::parse_lines(&raw);
    let joined = policy.effective_disallowed_packages(&self_pkg).join("
");
    env.new_string(joined).map(|s| s.into_raw()).unwrap_or(std::ptr::null_mut())
}


fn ctl_request_sync(data_dir: &std::path::Path, req: nexus_wire::CtlRequest) -> Result<nexus_wire::CtlResponse, String> {
    let ctl = nexus_cli::UnixCtl::new(data_dir, None);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    rt.block_on(async {
        tokio::time::timeout(std::time::Duration::from_secs(10), nexus_cli::call(&ctl, req))
            .await
            .map_err(|_| "核心没有回应（请确认 VPN 已启动）".to_string())?
            .map_err(|e| format!("无法连接核心：{e}"))
    })
}

fn route_list_json(data_dir: &std::path::Path) -> String {
    match ctl_request_sync(data_dir, nexus_wire::CtlRequest::RouteList) {
        Ok(nexus_wire::CtlResponse::Routes(rules)) => {
            let items: Vec<serde_json::Value> = rules
                .iter()
                .enumerate()
                .map(|(i, r)| {
                    serde_json::json!({
                        "index": i + 1,
                        "match": r.matcher.to_string(),
                        "outbound": r.outbound.to_string(),
                    })
                })
                .collect();
            serde_json::json!({ "ok": true, "rules": items }).to_string()
        }
        Ok(nexus_wire::CtlResponse::Error(e)) => serde_json::json!({ "ok": false, "error": e.message }).to_string(),
        Ok(other) => serde_json::json!({ "ok": false, "error": format!("未预期的回应：{other:?}") }).to_string(),
        Err(e) => serde_json::json!({ "ok": false, "error": e }).to_string(),
    }
}

fn route_import_json(
    data_dir: &std::path::Path,
    yaml: &str,
    via_exit: &str,
    socks5: &str,
    replace: bool,
    dry_run: bool,
) -> String {
    let opts = nexus_net_core::import::ImportOptions {
        default_exit: (!via_exit.trim().is_empty()).then(|| via_exit.trim().to_string()),
        default_socks5: (!socks5.trim().is_empty()).then(|| socks5.trim().to_string()),
    };
    let report = nexus_net_core::import::parse_clash_rules(yaml, &opts);
    let pre_audit = nexus_net_core::route::audit_rules(&report.imported);
    if dry_run {
        return serde_json::json!({
            "ok": true,
            "dry_run": true,
            "imported_count": report.imported.len(),
            "skipped": report.skipped,
            "notes": report.notes,
            "audit": pre_audit,
        })
        .to_string();
    }
    match ctl_request_sync(
        data_dir,
        nexus_wire::CtlRequest::RouteImport { rules: report.imported, replace },
    ) {
        Ok(nexus_wire::CtlResponse::RouteImported { total, audit }) => serde_json::json!({
            "ok": true,
            "dry_run": false,
            "total": total,
            "skipped": report.skipped,
            "notes": report.notes,
            "audit": audit,
        })
        .to_string(),
        Ok(nexus_wire::CtlResponse::Error(e)) => serde_json::json!({ "ok": false, "error": e.message }).to_string(),
        Ok(other) => serde_json::json!({ "ok": false, "error": format!("未预期的回应：{other:?}") }).to_string(),
        Err(e) => serde_json::json!({ "ok": false, "error": e }).to_string(),
    }
}

fn route_rollback_json(data_dir: &std::path::Path) -> String {
    match ctl_request_sync(data_dir, nexus_wire::CtlRequest::RouteRollback) {
        Ok(nexus_wire::CtlResponse::Ok) => serde_json::json!({ "ok": true }).to_string(),
        Ok(nexus_wire::CtlResponse::Error(e)) => serde_json::json!({ "ok": false, "error": e.message }).to_string(),
        Ok(other) => serde_json::json!({ "ok": false, "error": format!("未预期的回应：{other:?}") }).to_string(),
        Err(e) => serde_json::json!({ "ok": false, "error": e }).to_string(),
    }
}

fn route_rm_json(data_dir: &std::path::Path, index: u32) -> String {
    match ctl_request_sync(data_dir, nexus_wire::CtlRequest::RouteRm { index }) {
        Ok(nexus_wire::CtlResponse::Ok) => serde_json::json!({ "ok": true }).to_string(),
        Ok(nexus_wire::CtlResponse::Error(e)) => serde_json::json!({ "ok": false, "error": e.message }).to_string(),
        Ok(other) => serde_json::json!({ "ok": false, "error": format!("未预期的回应：{other:?}") }).to_string(),
        Err(e) => serde_json::json!({ "ok": false, "error": e }).to_string(),
    }
}

fn route_explain_json(data_dir: &std::path::Path, target: &str) -> String {
    let trimmed = target.trim();
    let normalized = if trimmed.contains(':') { trimmed.to_string() } else { format!("{trimmed}:443") };
    if nexus_net_core::Target::parse(&normalized).is_none() {
        return serde_json::json!({ "ok": false, "error": format!("目标格式不合法：{trimmed}（示例：claude.ai:443）") }).to_string();
    }
    match ctl_request_sync(data_dir, nexus_wire::CtlRequest::RouteExplain { target: normalized.clone() }) {
        Ok(nexus_wire::CtlResponse::Explained(exp)) => {
            let summary = match exp {
                nexus_wire::RouteExplanation::Service { device, service } => {
                    format!("目标 {normalized} → Nexus 私有服务 {device}/{service}")
                }
                nexus_wire::RouteExplanation::BadInternalName => {
                    format!("目标 {normalized} → 非法的 .internal 服务名（应为 <服务>.<设备>.internal）")
                }
                nexus_wire::RouteExplanation::Rule { index, rule } => {
                    format!("目标 {normalized} → 命中第 {index} 条规则（{}）→ {}", rule.matcher, rule.outbound)
                }
                nexus_wire::RouteExplanation::Default { outbound } => {
                    format!("目标 {normalized} → 未命中规则，走默认动作：{outbound}")
                }
            };
            serde_json::json!({ "ok": true, "summary": summary }).to_string()
        }
        Ok(nexus_wire::CtlResponse::Error(e)) => serde_json::json!({ "ok": false, "error": e.message }).to_string(),
        Ok(other) => serde_json::json!({ "ok": false, "error": format!("未预期的回应：{other:?}") }).to_string(),
        Err(e) => serde_json::json!({ "ok": false, "error": e }).to_string(),
    }
}

fn route_audit_json(data_dir: &std::path::Path) -> String {
    match ctl_request_sync(data_dir, nexus_wire::CtlRequest::RouteAudit) {
        Ok(nexus_wire::CtlResponse::RouteAudit(audit)) => {
            serde_json::json!({ "ok": true, "clean": audit.is_clean(), "issues": audit.issues }).to_string()
        }
        Ok(nexus_wire::CtlResponse::Error(e)) => serde_json::json!({ "ok": false, "error": e.message }).to_string(),
        Ok(other) => serde_json::json!({ "ok": false, "error": format!("未预期的回应：{other:?}") }).to_string(),
        Err(e) => serde_json::json!({ "ok": false, "error": e }).to_string(),
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_link_bewith_nexus_NexusCore_nativeRouteList(
    mut env: JNIEnv,
    _class: JClass,
    data_dir: JString,
) -> jstring {
    let dir = string(&mut env, &data_dir);
    let json = route_list_json(std::path::Path::new(&dir));
    env.new_string(json).map(|s| s.into_raw()).unwrap_or(std::ptr::null_mut())
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_link_bewith_nexus_NexusCore_nativeRouteImport(
    mut env: JNIEnv,
    _class: JClass,
    data_dir: JString,
    yaml: JString,
    via_exit: JString,
    socks5: JString,
    replace: jboolean,
    dry_run: jboolean,
) -> jstring {
    let dir = string(&mut env, &data_dir);
    let yaml = string(&mut env, &yaml);
    let via_exit = string(&mut env, &via_exit);
    let socks5 = string(&mut env, &socks5);
    let json = route_import_json(std::path::Path::new(&dir), &yaml, &via_exit, &socks5, replace != 0, dry_run != 0);
    env.new_string(json).map(|s| s.into_raw()).unwrap_or(std::ptr::null_mut())
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_link_bewith_nexus_NexusCore_nativeRouteRollback(
    mut env: JNIEnv,
    _class: JClass,
    data_dir: JString,
) -> jstring {
    let dir = string(&mut env, &data_dir);
    let json = route_rollback_json(std::path::Path::new(&dir));
    env.new_string(json).map(|s| s.into_raw()).unwrap_or(std::ptr::null_mut())
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_link_bewith_nexus_NexusCore_nativeRouteRm(
    mut env: JNIEnv,
    _class: JClass,
    data_dir: JString,
    index: jint,
) -> jstring {
    let dir = string(&mut env, &data_dir);
    let json = route_rm_json(std::path::Path::new(&dir), index.max(0) as u32);
    env.new_string(json).map(|s| s.into_raw()).unwrap_or(std::ptr::null_mut())
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_link_bewith_nexus_NexusCore_nativeRouteExplain(
    mut env: JNIEnv,
    _class: JClass,
    data_dir: JString,
    target: JString,
) -> jstring {
    let dir = string(&mut env, &data_dir);
    let target = string(&mut env, &target);
    let json = route_explain_json(std::path::Path::new(&dir), &target);
    env.new_string(json).map(|s| s.into_raw()).unwrap_or(std::ptr::null_mut())
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_link_bewith_nexus_NexusCore_nativeRouteAudit(
    mut env: JNIEnv,
    _class: JClass,
    data_dir: JString,
) -> jstring {
    let dir = string(&mut env, &data_dir);
    let json = route_audit_json(std::path::Path::new(&dir));
    env.new_string(json).map(|s| s.into_raw()).unwrap_or(std::ptr::null_mut())
}


fn dns_explain_json(data_dir: &std::path::Path, domain: &str) -> String {
    match ctl_request_sync(data_dir, nexus_wire::CtlRequest::DnsExplain { domain: domain.trim().to_string() }) {
        Ok(nexus_wire::CtlResponse::DnsExplained(d)) => serde_json::json!({ "ok": true, "summary": format!("{d:?}") }).to_string(),
        Ok(nexus_wire::CtlResponse::Error(e)) => serde_json::json!({ "ok": false, "error": e.message }).to_string(),
        Ok(other) => serde_json::json!({ "ok": false, "error": format!("未预期的回应：{other:?}") }).to_string(),
        Err(e) => serde_json::json!({ "ok": false, "error": e }).to_string(),
    }
}

fn flow_recent_json(data_dir: &std::path::Path, limit: u32) -> String {
    match ctl_request_sync(data_dir, nexus_wire::CtlRequest::FlowRecent { limit }) {
        Ok(nexus_wire::CtlResponse::Flows(events)) => serde_json::json!({ "ok": true, "events": events }).to_string(),
        Ok(nexus_wire::CtlResponse::Error(e)) => serde_json::json!({ "ok": false, "error": e.message }).to_string(),
        Ok(other) => serde_json::json!({ "ok": false, "error": format!("未预期的回应：{other:?}") }).to_string(),
        Err(e) => serde_json::json!({ "ok": false, "error": e }).to_string(),
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_link_bewith_nexus_NexusCore_nativeDnsExplain(
    mut env: JNIEnv,
    _class: JClass,
    data_dir: JString,
    domain: JString,
) -> jstring {
    let dir = string(&mut env, &data_dir);
    let domain = string(&mut env, &domain);
    let json = dns_explain_json(std::path::Path::new(&dir), &domain);
    env.new_string(json).map(|s| s.into_raw()).unwrap_or(std::ptr::null_mut())
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_link_bewith_nexus_NexusCore_nativeFlowRecent(
    mut env: JNIEnv,
    _class: JClass,
    data_dir: JString,
    limit: jint,
) -> jstring {
    let dir = string(&mut env, &data_dir);
    let json = flow_recent_json(std::path::Path::new(&dir), limit.max(1) as u32);
    env.new_string(json).map(|s| s.into_raw()).unwrap_or(std::ptr::null_mut())
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
