//! 网页节点核心在浏览器里的绑定。页面（web/）通过这些接口登记、登录、打开终端；
//! 通行密钥由页面调用浏览器的 WebAuthn 接口完成，这里只处理协议。
//!
//! 浏览器里只能经中继通信：每个浏览器启动时生成一个只在内存里的临时身份，页面关闭即消失。
#![forbid(unsafe_code)]

use js_sys::Function;
use nexus_core_proto::webauthn::WebAuthnAssertion;
use nexus_core_proto::Timestamp;
use nexus_fabric::Transport;
use nexus_fabric_iroh::{Config, IrohTransport, Relay};
use nexus_web_core::{EnrollLink, EntryHandle, HardwareKind, HardwareState, Notice, TerminalEnd, TerminalUi, UiEvent, WebNode};
use nexus_wire::{NewCredential, Size, WireError};
use std::cell::RefCell;
use std::rc::Rc;
use tokio::sync::mpsc;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::future_to_promise;

fn js_err(e: WireError) -> JsError {
    JsError::new(&e.message)
}

/// 页面加载后调用一次。
#[wasm_bindgen(start)]
pub fn start() {
    #[cfg(target_arch = "wasm32")]
    console_error_panic_hook::set_once();
}

/// 这个浏览器：一个只在内存里的临时身份。先签名再连接：签名时页面还不知道该经哪个中继，
/// 中继来自登记链接或通行密钥里的入口信息，所以连接（建立端点）推迟到登记或登录时。
#[wasm_bindgen]
pub struct Browser {
    seed: [u8; 32],
    /// 用过之后（登录过）不能再用。
    used: Rc<RefCell<bool>>,
    /// 入口信息里没有中继时使用的中继（通常是页面所在的站点）。
    default_relay: String,
}

async fn connect(seed: [u8; 32], relay: String) -> Result<IrohTransport, JsError> {
    let config = Config { relay: Relay::Custom(relay), discovery: false, bind: None, direct: false, scheduling: true, rendezvous: None };
    IrohTransport::bind(seed, config).await.map_err(|e| JsError::new(&format!("连不上中继：{e}")))
}

fn entry_of(link: &EnrollLink) -> EntryHandle {
    EntryHandle { node: link.node, relay: nexus_fabric_iroh::hint_relay(&link.hint) }
}

#[wasm_bindgen]
impl Browser {
    /// 生成临时身份。`default_relay` 例如 https://bewith.link。
    #[wasm_bindgen(constructor)]
    pub fn new(default_relay: String) -> Result<Browser, JsError> {
        let mut seed = [0u8; 32];
        getrandom_fill(&mut seed)?;
        Ok(Browser { seed, used: Rc::new(RefCell::new(false)), default_relay })
    }

    /// 签名时间为 `issued_at_ms` 的登录挑战：交给 `navigator.credentials.get` 签名。
    pub fn challenge(&self, issued_at_ms: f64) -> Vec<u8> {
        let me = nexus_fabric_iroh::node_id_from_seed(&self.seed);
        nexus_core_proto::webauthn::login_challenge(Timestamp(issued_at_ms as u64), me).to_vec()
    }

    /// 登记链接（或其中 `#` 之后的部分）对应的入口信息，作为新通行密钥的用户标识。
    pub fn user_handle(link: String) -> Result<Vec<u8>, JsError> {
        let link = EnrollLink::parse(&link).map_err(|m| JsError::new(&m))?;
        entry_of(&link).encode().map_err(|m| JsError::new(&m))
    }

    /// 把新建的通行密钥交给管理设备登记。Promise 的结果是它在网络里的名字。
    pub fn enroll(&self, link: String, credential_id: Vec<u8>, spki: Vec<u8>, rp_id: String) -> js_sys::Promise {
        let (seed, default_relay) = (self.seed, self.default_relay.clone());
        future_to_promise(async move {
            let link = EnrollLink::parse(&link).map_err(|m| JsError::new(&m))?;
            let relay = entry_of(&link).relay.unwrap_or(default_relay);
            let t = connect(seed, relay).await?;
            let label = nexus_web_core::enroll(&t, &link, NewCredential { credential_id, spki, rp_id }).await.map_err(js_err)?;
            Ok(JsValue::from(label))
        })
    }

    /// 用通行密钥的签名登录，Promise 的结果是 [`Session`]。`user_handle` 是通行密钥返回的用户标识。
    /// 无论成败，之后这个浏览器都不能再登录（换一个新的）。
    #[allow(clippy::too_many_arguments)]
    pub fn login(
        &self,
        user_handle: Vec<u8>,
        issued_at_ms: f64,
        credential_id: Vec<u8>,
        client_data_json: Vec<u8>,
        authenticator_data: Vec<u8>,
        signature: Vec<u8>,
    ) -> js_sys::Promise {
        let (seed, default_relay, used) = (self.seed, self.default_relay.clone(), self.used.clone());
        future_to_promise(async move {
            if used.replace(true) {
                return Err(JsError::new("这个浏览器已经登录过").into());
            }
            let entry = EntryHandle::decode(&user_handle).ok_or_else(|| JsError::new("这个通行密钥不是 Nexus 的"))?;
            let relay = entry.relay.unwrap_or(default_relay);
            let hint = nexus_fabric_iroh::relay_hint(entry.node, &relay).map_err(|e: String| JsError::new(&e))?;
            let t = connect(seed, relay).await?;
            t.add_hint(entry.node, &hint);
            let assertion = WebAuthnAssertion { credential_id, client_data_json, authenticator_data, signature };
            let node = WebNode::login(t, entry.node, Timestamp(issued_at_ms as u64), assertion).await.map_err(js_err)?;
            Ok(JsValue::from(Session { node: Rc::new(node), hardware: HardwareState::new() }))
        })
    }
}

fn getrandom_fill(buf: &mut [u8]) -> Result<(), JsError> {
    #[cfg(target_arch = "wasm32")]
    {
        getrandom::fill(buf).map_err(|e| JsError::new(&e.to_string()))
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = buf;
        Err(JsError::new("只在浏览器里运行"))
    }
}

/// 登录之后。
#[wasm_bindgen]
pub struct Session {
    node: Rc<WebNode<IrohTransport>>,
    hardware: HardwareState,
}

#[wasm_bindgen]
impl Session {
    /// 通行密钥在网络里的名字。
    pub fn label(&self) -> String {
        self.node.label().to_string()
    }

    /// 登录到期的时间（毫秒）。
    pub fn expires_at_ms(&self) -> f64 {
        self.node.expires_at().0 as f64
    }

    /// 显式开启一项硬件能力（M8.3：camera / microphone / location）。
    pub fn hardware_enable(&self, kind: String) -> Result<(), JsError> {
        let k = HardwareKind::parse(&kind).ok_or_else(|| JsError::new(&format!("未知的硬件能力：{kind}")))?;
        self.hardware.enable(k);
        Ok(())
    }

    /// 关闭一项硬件能力（M8.3）。
    pub fn hardware_disable(&self, kind: String) -> Result<(), JsError> {
        let k = HardwareKind::parse(&kind).ok_or_else(|| JsError::new(&format!("未知的硬件能力：{kind}")))?;
        self.hardware.disable(k);
        Ok(())
    }

    /// 清空全部硬件授权（离开或关闭页面时调用，R32）。
    pub fn hardware_clear(&self) {
        self.hardware.clear();
    }

    /// 返回当前已显式同意开放的硬件能力（JSON 数组）。
    pub fn hardware_enabled(&self) -> String {
        let list: Vec<&str> = self.hardware.enabled_list().into_iter().map(HardwareKind::as_str).collect();
        serde_json::to_string(&list).unwrap_or_else(|_| "[]".into())
    }

    /// 返回当前正在使用硬件的调用方（JSON 数组，每项有 caller、kind）。
    pub fn hardware_active(&self) -> String {
        let list: Vec<_> = self
            .hardware
            .active_users()
            .into_iter()
            .map(|(caller, k)| serde_json::json!({ "caller": caller, "kind": k.as_str() }))
            .collect();
        serde_json::to_string(&list).unwrap_or_else(|_| "[]".into())
    }

    /// 能看到的设备的名字（JSON 数组）。
    pub fn devices(&self) -> String {
        let names: Vec<&str> = self.node.devices().iter().map(|d| d.name.as_str()).collect();
        serde_json::to_string(&names).expect("名字总能编码")
    }

    /// 一台设备上的会话：Promise 的结果是 JSON 数组，每项有 name、clients。
    pub fn sessions(&self, device: String) -> js_sys::Promise {
        let node = self.node.clone();
        future_to_promise(async move {
            let list = node.session_list(&device).await.map_err(js_err)?;
            let list: Vec<_> = list.iter().map(|s| serde_json::json!({ "name": s.name, "clients": s.clients })).collect();
            Ok(JsValue::from(serde_json::Value::Array(list).to_string()))
        })
    }

    /// 能读的工作区（M8.1）：Promise 的结果是 JSON 数组，每项有 name、device（保存它的存储设备）。
    pub fn workspaces(&self) -> js_sys::Promise {
        let node = self.node.clone();
        future_to_promise(async move {
            let list = node.drive_workspaces().await.map_err(js_err)?;
            let list: Vec<_> = list.iter().map(|w| serde_json::json!({ "name": w.name, "device": w.device })).collect();
            Ok(JsValue::from(serde_json::Value::Array(list).to_string()))
        })
    }

    /// 工作区里的一个目录：Promise 的结果是 JSON 数组，每项有 name、kind（file、dir、symlink）、size、target。
    pub fn list(&self, ws: String, path: String) -> js_sys::Promise {
        let node = self.node.clone();
        future_to_promise(async move {
            let list = node.drive_list(&ws, &path).await.map_err(js_err)?;
            let list: Vec<_> =
                list.iter().map(|e| serde_json::json!({ "name": e.name, "kind": e.kind, "size": e.size, "target": e.target })).collect();
            Ok(JsValue::from(serde_json::Value::Array(list).to_string()))
        })
    }

    /// 读工作区里文件开头的最多 `max` 字节：Promise 的结果是 [`FileData`]。
    pub fn read(&self, ws: String, path: String, max: f64) -> js_sys::Promise {
        let node = self.node.clone();
        future_to_promise(async move {
            let f = node.drive_read(&ws, &path, max as u64).await.map_err(js_err)?;
            Ok(JsValue::from(FileData { size: f.size, truncated: f.truncated, bytes: f.bytes }))
        })
    }

    /// 设备上能用的服务（M8.2）：Promise 的结果是 JSON 数组，每项有 name、proto。
    pub fn services(&self, device: String) -> js_sys::Promise {
        let node = self.node.clone();
        future_to_promise(async move {
            let list = node.services(&device).await.map_err(js_err)?;
            let list: Vec<_> = list.iter().map(|s| serde_json::json!({ "name": s.name, "proto": s.proto })).collect();
            Ok(JsValue::from(serde_json::Value::Array(list).to_string()))
        })
    }

    /// 经 Nexus 向设备上的网页服务发一个 HTTP 请求：`headers` 是 JSON 的 [[名字, 值], ...]。
    /// Promise 的结果是 [`HttpResult`]。
    pub fn http(&self, device: String, service: String, method: String, path: String, headers: String, body: Vec<u8>) -> js_sys::Promise {
        let node = self.node.clone();
        future_to_promise(async move {
            let headers: Vec<(String, String)> = serde_json::from_str(&headers).map_err(|e| JsValue::from_str(&format!("请求头不合法：{e}")))?;
            let r = node.http(&device, &service, &method, &path, &headers, &body).await.map_err(js_err)?;
            Ok(JsValue::from(HttpResult { status: r.status, reason: r.reason, headers: r.headers, body: r.body }))
        })
    }

    /// 打开一台设备上的终端会话（不存在时创建）。
    ///
    /// `on_output(Uint8Array)` 显示输出；`on_notice(string)` 在连接中断（"reconnecting"）和恢复（"reconnected"）时调用；
    /// `on_end(code, message)` 在终端结束时调用一次：会话退出时 code 是退出码，用户离开时是 0，出错时是 -1 并带原因。
    #[allow(clippy::too_many_arguments)]
    pub fn open(&self, device: String, session: String, cols: u16, rows: u16, on_output: Function, on_notice: Function, on_end: Function) -> Terminal {
        let (tx, rx) = mpsc::unbounded_channel();
        let node = self.node.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let mut ui = JsUi { on_output, on_notice, rx };
            let size = Size { cols, rows };
            let (code, message) = match node.run_terminal(&device, &session, size, &mut ui).await {
                Ok(TerminalEnd::Exited(code)) => (code.unwrap_or(0), String::new()),
                Ok(TerminalEnd::Left) => (0, String::new()),
                Err(e) => (-1, e.message),
            };
            let _ = on_end.call2(&JsValue::NULL, &JsValue::from(code), &JsValue::from(message));
        });
        Terminal { tx }
    }
}

/// 网页服务的回应。
#[wasm_bindgen]
pub struct HttpResult {
    status: u16,
    reason: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

#[wasm_bindgen]
impl HttpResult {
    pub fn status(&self) -> u16 {
        self.status
    }

    pub fn reason(&self) -> String {
        self.reason.clone()
    }

    /// JSON 的 [[名字, 值], ...]。
    pub fn headers(&self) -> String {
        serde_json::to_string(&self.headers).unwrap_or_else(|_| "[]".into())
    }

    pub fn body(&self) -> Vec<u8> {
        self.body.clone()
    }
}

/// 读到的文件：总大小、是否没读完、读到的内容。
#[wasm_bindgen]
pub struct FileData {
    size: u64,
    truncated: bool,
    bytes: Vec<u8>,
}

#[wasm_bindgen]
impl FileData {
    pub fn size(&self) -> f64 {
        self.size as f64
    }

    pub fn truncated(&self) -> bool {
        self.truncated
    }

    pub fn bytes(&self) -> Vec<u8> {
        self.bytes.clone()
    }
}

/// 一个打开的终端：把按键、尺寸变化和离开送给网页节点核心。
#[wasm_bindgen]
pub struct Terminal {
    tx: mpsc::UnboundedSender<UiEvent>,
}

#[wasm_bindgen]
impl Terminal {
    /// 用户的输入（已编码成终端的字节，xterm.js 的 onData 给出的字符串按 UTF-8 编码）。
    pub fn keys(&self, data: Vec<u8>) {
        let _ = self.tx.send(UiEvent::Keys(data));
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        let _ = self.tx.send(UiEvent::Resize(Size { cols, rows }));
    }

    /// 离开：会话继续运行。
    pub fn leave(&self) {
        let _ = self.tx.send(UiEvent::Leave);
    }
}

struct JsUi {
    on_output: Function,
    on_notice: Function,
    rx: mpsc::UnboundedReceiver<UiEvent>,
}

impl TerminalUi for JsUi {
    fn output(&mut self, bytes: &[u8]) {
        let _ = self.on_output.call1(&JsValue::NULL, &js_sys::Uint8Array::from(bytes));
    }

    fn notice(&mut self, notice: Notice) {
        let text = match notice {
            Notice::Reconnecting => "reconnecting",
            Notice::Reconnected => "reconnected",
        };
        let _ = self.on_notice.call1(&JsValue::NULL, &JsValue::from_str(text));
    }

    async fn event(&mut self) -> UiEvent {
        // 页面没了（终端对象被丢弃）就是离开。
        self.rx.recv().await.unwrap_or(UiEvent::Leave)
    }
}
