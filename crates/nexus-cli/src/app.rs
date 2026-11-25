//! 应用界面的逻辑（M11.2，R79、R80）：Android 应用的界面「现在该显示什么」。
//!
//! 界面经控制通道和本机的核心说话，用的是命令行已有的控制请求（`List`、`JoinStart`），不改协议。
//! 逻辑都在这里，仿真和真实后端都能测；Java 一侧只负责按 [`Screen`] 画图、定时调用 [`refresh`]。

use crate::{call, Ctl};
use nexus_wire::{CtlRequest, CtlResponse, ErrorCode};
use serde::{Deserialize, Serialize};

/// 离到期不到这么久时，界面上的加入码换成新的：免得管理设备那边刚输完就过期。
pub const RENEW_BEFORE_MS: u64 = 30_000;

/// 界面要显示的内容。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Screen {
    /// 核心还没起来（控制套接字连不上）。
    Starting { message: String },
    /// 本机已经在网络里。
    Joined { name: String, devices: Vec<Device> },
    /// 还没加入：显示加入码，等管理设备批准。
    Joining(JoinView),
    /// 其他失败，原样显示。
    Failed { message: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    pub name: String,
    pub online: bool,
    pub is_self: bool,
    pub admin: bool,
}

/// 一个加入码，以及画它需要的一切。界面下次刷新时原样交回（[`refresh`] 的 `held`）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinView {
    /// 短加入码（`XXXXX-XXXXX`）；没有碰头服务时为空。
    pub short_code: Option<String>,
    /// 短加入码不可用的原因。
    pub short_code_error: Option<String>,
    /// 长加入码（`nxj2-…`）。
    pub code: String,
    /// 二维码里的文字：长加入码的大写形式（R80）。
    pub qr_text: String,
    pub qr: Qr,
    /// 到期时间（本机时钟，毫秒）。
    pub expires_at_ms: u64,
}

/// 二维码的点阵：`rows[y]` 的第 x 个字符是 `1`（深色）或 `0`。不含白边。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Qr {
    pub size: usize,
    pub rows: Vec<String>,
}

impl Qr {
    pub fn dark(&self, x: usize, y: usize) -> bool {
        self.rows.get(y).and_then(|r| r.as_bytes().get(x)) == Some(&b'1')
    }
}

/// 二维码里放的文字：长加入码的大写形式。大写字母、数字和 `-` 都在二维码的字母数字模式里，同样的内容二维码更小；
/// 长加入码解码时不区分大小写。
pub fn qr_text(code: &str) -> String {
    code.trim().to_ascii_uppercase()
}

/// 生成二维码的点阵（纠错等级 M）。
pub fn qr(text: &str) -> Result<Qr, String> {
    let code = qrcode::QrCode::with_error_correction_level(text.as_bytes(), qrcode::EcLevel::M).map_err(|e| format!("无法生成二维码：{e}"))?;
    let size = code.width();
    let colors = code.to_colors();
    let rows = colors
        .chunks(size)
        .map(|row| row.iter().map(|c| if *c == qrcode::Color::Dark { '1' } else { '0' }).collect())
        .collect();
    Ok(Qr { size, rows })
}

/// 界面刷新一次：本机已加入就显示已加入；否则显示加入码——交回的 `held` 还有效、也没要求换时原样用它，
/// 不重新发起加入（重新发起会让旧的短加入码失效）。`now_ms` 是本机现在的时间。
pub async fn refresh<C: Ctl>(ctl: &C, held: Option<JoinView>, renew: bool, now_ms: u64) -> Screen {
    let entries = match call(ctl, CtlRequest::List).await {
        Err(e) => return Screen::Starting { message: format!("正在启动 Nexus…（{e}）") },
        Ok(CtlResponse::List(entries)) => entries,
        Ok(CtlResponse::Error(e)) => return Screen::Failed { message: e.message },
        Ok(other) => return Screen::Failed { message: format!("核心的回应无法理解：{other:?}") },
    };
    if let Some(screen) = joined(&entries) {
        return screen;
    }
    if let Some(view) = held.filter(|h| !renew && h.expires_at_ms >= now_ms.saturating_add(RENEW_BEFORE_MS)) {
        return Screen::Joining(view);
    }
    match call(ctl, CtlRequest::JoinStart).await {
        Err(e) => Screen::Starting { message: format!("正在启动 Nexus…（{e}）") },
        Ok(CtlResponse::JoinCode { code, short_code, short_code_error, expires_at }) => {
            let text = qr_text(&code);
            match qr(&text) {
                Ok(qr) => Screen::Joining(JoinView { short_code, short_code_error, code, qr_text: text, qr, expires_at_ms: expires_at.0 }),
                Err(message) => Screen::Failed { message },
            }
        }
        // 刚好在两次请求之间被批准了。
        Ok(CtlResponse::Error(e)) if e.code == ErrorCode::InvalidState => match call(ctl, CtlRequest::List).await {
            Ok(CtlResponse::List(entries)) => joined(&entries).unwrap_or(Screen::Failed { message: e.message }),
            _ => Screen::Failed { message: e.message },
        },
        Ok(CtlResponse::Error(e)) => Screen::Failed { message: e.message },
        Ok(other) => Screen::Failed { message: format!("核心的回应无法理解：{other:?}") },
    }
}

fn joined(entries: &[nexus_wire::ListEntry]) -> Option<Screen> {
    let me = entries.iter().find(|e| e.is_self && e.report.role.is_some())?;
    let devices = entries
        .iter()
        .map(|e| Device {
            name: e.report.name.clone(),
            online: e.is_self || e.report.online,
            is_self: e.is_self,
            admin: e.report.role == Some(nexus_core_proto::roster::DeviceRole::Admin),
        })
        .collect();
    Some(Screen::Joined { name: me.report.name.clone(), devices })
}

#[cfg(test)]
mod tests {
    // covers: R79, R80
    use super::*;
    use nexus_runtime::CtlClient;
    use std::io;

    const CODE: &str = "nxj2-aebagbafaydqqcikbmga2dqpcaireeyuculbogazdinryhi6d4qa";

    fn scan(qr: &Qr) -> String {
        // 画成位图（每格 4 像素，白边 4 格），交给一个独立的识读库。
        let (scale, margin) = (4, 4);
        let px = (qr.size + 2 * margin) * scale;
        let img = rqrr::PreparedImage::prepare_from_greyscale(px, px, |x, y| {
            let (mx, my) = (x / scale, y / scale);
            let dark = mx >= margin && my >= margin && qr.dark(mx - margin, my - margin);
            if dark { 0 } else { 255 }
        });
        let mut img = img;
        let grids = img.detect_grids();
        assert_eq!(grids.len(), 1, "应当识别出一个二维码");
        grids[0].decode().expect("二维码无法解码").1
    }

    // FP-11.2-07：独立的识读库读出的文字和 qr_text 完全相同
    #[test]
    fn qr_scans_back_to_the_same_text() {
        let text = qr_text(CODE);
        assert_eq!(text, CODE.to_ascii_uppercase());
        let q = qr(&text).unwrap();
        assert_eq!(q.rows.len(), q.size);
        assert!(q.rows.iter().all(|r| r.len() == q.size && r.bytes().all(|b| b == b'0' || b == b'1')));
        assert_eq!(scan(&q), text);
    }

    // FP-11.2-07：大写走字母数字模式，二维码不比小写的大
    #[test]
    fn uppercase_code_makes_a_smaller_or_equal_qr() {
        let long = format!("nxj2-{}", "abcdefghijkmnpqrstuvwxyz234567".repeat(5));
        let upper = qr(&qr_text(&long)).unwrap().size;
        let lower = qr(&long).unwrap().size;
        assert!(upper < lower, "大写 {upper}，小写 {lower}");
    }

    // FP-11.2-07：扫出来的文字带空白也能当长码用
    #[test]
    fn scanned_text_is_a_valid_long_code() {
        let code = nexus_wire::JoinCode { node_id: nexus_core_proto::NodeId([7; 32]), nonce: [9; 16], name: "phone".into(), addr: "0a0b".into() };
        let text = qr_text(&code.encode());
        let scanned = format!("  {text}\n");
        assert!(nexus_wire::JoinCode::looks_like(&scanned));
        assert_eq!(nexus_wire::JoinCode::decode(&scanned), Some(code));
    }

    struct Down;

    impl Ctl for Down {
        async fn open(&self, _req: CtlRequest) -> io::Result<CtlClient> {
            Err(io::Error::new(io::ErrorKind::NotFound, "没有控制套接字"))
        }
    }

    // FP-11.2-06：核心还没起来
    #[tokio::test]
    async fn starting_when_the_core_is_not_up() {
        match refresh(&Down, None, false, 0).await {
            Screen::Starting { message } => assert!(message.contains("正在启动"), "{message}"),
            other => panic!("应当是 Starting：{other:?}"),
        }
    }

    // 界面和 JNI 之间是 JSON：字段名固定。
    #[test]
    fn screen_json_shape() {
        let view = JoinView {
            short_code: Some("7KQ4M-9XZ21".into()),
            short_code_error: None,
            code: CODE.into(),
            qr_text: qr_text(CODE),
            qr: Qr { size: 1, rows: vec!["1".into()] },
            expires_at_ms: 5,
        };
        let v = serde_json::to_value(Screen::Joining(view.clone())).unwrap();
        assert_eq!(v["state"], "joining");
        assert_eq!(v["short_code"], "7KQ4M-9XZ21");
        assert_eq!(v["qr"]["rows"][0], "1");
        let back: Screen = serde_json::from_value(v).unwrap();
        assert_eq!(back, Screen::Joining(view));
        let v = serde_json::to_value(Screen::Joined { name: "phone".into(), devices: vec![] }).unwrap();
        assert_eq!(v["state"], "joined");
        assert_eq!(serde_json::to_value(Screen::Starting { message: "x".into() }).unwrap()["state"], "starting");
    }
}
