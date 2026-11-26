use crate::{call, doc, fail_wire, Ctl, Sink, JSON_SCHEMA};
use nexus_core_proto::PathKind;
use nexus_wire::{
    CtlRequest, CtlResponse, DevicePaths, ErrorCode, ListEntry, PathInfo, RouteExplanation, StatusReport, TunReport,
    WIRE_MINOR, WIRE_VERSION,
};
use serde_json::{json, Value};
use std::cmp::Ordering;
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CheckStatus {
    Ok,
    Warn,
    Fail,
}

impl CheckStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            CheckStatus::Ok => "ok",
            CheckStatus::Warn => "warn",
            CheckStatus::Fail => "fail",
        }
    }

    fn badge(self) -> &'static str {
        match self {
            CheckStatus::Ok => "OK",
            CheckStatus::Warn => "WARN",
            CheckStatus::Fail => "FAIL",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckItem {
    pub name: String,
    pub status: CheckStatus,
    pub detail: String,
    pub suggestion: Option<String>,
}

impl CheckItem {
    fn ok(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self { name: name.into(), status: CheckStatus::Ok, detail: detail.into(), suggestion: None }
    }

    fn warn(name: impl Into<String>, detail: impl Into<String>, suggestion: impl Into<String>) -> Self {
        Self { name: name.into(), status: CheckStatus::Warn, detail: detail.into(), suggestion: Some(suggestion.into()) }
    }

    fn fail(name: impl Into<String>, detail: impl Into<String>, suggestion: impl Into<String>) -> Self {
        Self { name: name.into(), status: CheckStatus::Fail, detail: detail.into(), suggestion: Some(suggestion.into()) }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DoctorReport {
    pub overall: CheckStatus,
    pub checks: Vec<CheckItem>,
}

pub struct DoctorSnapshot {
    pub cli_version: String,
    pub daemon: Result<StatusReport, String>,
    pub list: Option<Result<Vec<ListEntry>, String>>,
    pub paths: Option<Result<Vec<DevicePaths>, String>>,
    pub tun: Option<Result<Option<TunReport>, String>>,
    pub route_explain: Option<Result<RouteExplanation, String>>,
}

pub fn evaluate(snap: &DoctorSnapshot) -> DoctorReport {
    let mut checks = Vec::new();
    let local = match &snap.daemon {
        Err(err) => {
            checks.push(CheckItem::fail(
                "daemon",
                format!("连不上本机常驻程序（{err}）"),
                "启动本机常驻程序（nexus daemon &），或确认 --data-dir / NEXUS_DATA_DIR 与常驻程序一致",
            ));
            return finish(checks);
        }
        Ok(st) => st,
    };

    checks.push(check_daemon(local, &snap.cli_version));

    let local_version = local.info.as_ref().map(|i| i.version.as_str()).unwrap_or(snap.cli_version.as_str());
    let joined = local.info.as_ref().and_then(|i| i.roster_version).is_some();

    let mut online_peers: Vec<&ListEntry> = Vec::new();
    match &snap.list {
        Some(Ok(entries)) => {
            let mut peers: Vec<&ListEntry> = entries.iter().filter(|e| !e.is_self).collect();
            peers.sort_by(|a, b| a.report.name.cmp(&b.report.name));
            if peers.is_empty() {
                let (detail, suggestion) = if joined {
                    (
                        "名册中只有本机，尚未加入其他设备",
                        "在其他设备上运行 nexus join，再在本机运行 nexus device approve 批准加入",
                    )
                } else {
                    (
                        "本机尚未加入任何网络，没有其他已知设备",
                        "运行 nexus init 建立网络，或运行 nexus join 加入已有网络",
                    )
                };
                checks.push(CheckItem::warn("devices", detail, suggestion));
            } else {
                for peer in &peers {
                    if peer.report.online {
                        let detail = match peer.report.rtt_ms {
                            Some(ms) => format!("设备 {} 在线（往返 {ms} ms）", peer.report.name),
                            None => format!("设备 {} 在线", peer.report.name),
                        };
                        checks.push(CheckItem::ok(format!("device:{}", peer.report.name), detail));
                        online_peers.push(peer);
                    } else {
                        checks.push(CheckItem::fail(
                            format!("device:{}", peer.report.name),
                            format!("设备 {} 离线", peer.report.name),
                            format!("确认 {} 已运行 nexus daemon，并检查网络连接或中继设置", peer.report.name),
                        ));
                    }
                }
            }
        }
        Some(Err(err)) => {
            checks.push(CheckItem::warn(
                "devices",
                format!("无法获取设备列表（{err}）"),
                "运行 nexus ls 检查名册与设备可达性",
            ));
        }
        None => {
            checks.push(CheckItem::warn("devices", "未获取到设备列表", "运行 nexus ls 检查设备列表"));
        }
    }

    let path_map: Result<HashMap<&str, &[PathInfo]>, Option<&str>> = match &snap.paths {
        Some(Ok(list)) => Ok(list.iter().map(|d| (d.device.as_str(), d.paths.as_slice())).collect()),
        Some(Err(err)) => Err(Some(err.as_str())),
        None => Err(None),
    };
    for peer in &online_peers {
        checks.push(check_peer_path(peer, &path_map));
    }

    checks.push(check_version(local_version, &online_peers));
    checks.push(check_name_resolution(&snap.tun, &snap.route_explain, online_peers.first().map(|p| p.report.name.as_str()).unwrap_or(&local.name)));

    finish(checks)
}

fn finish(checks: Vec<CheckItem>) -> DoctorReport {
    let overall = checks.iter().map(|c| c.status).max().unwrap_or(CheckStatus::Ok);
    DoctorReport { overall, checks }
}

fn check_daemon(local: &StatusReport, cli_version: &str) -> CheckItem {
    let Some(info) = &local.info else {
        return CheckItem::warn("daemon", "本机常驻程序可达，但未返回版本信息", "运行 nexus status 检查本机常驻程序状态");
    };
    if info.version != cli_version {
        return CheckItem::warn(
            "daemon",
            format!("本机 {} 常驻程序可达，但版本 {} 与命令行 {} 不一致", local.name, info.version, cli_version),
            "重启或升级本机常驻程序，使常驻程序与命令行版本一致（nexus upgrade）",
        );
    }
    CheckItem::ok(
        "daemon",
        format!("本机 {} 常驻程序可达，版本 {}（协议 {WIRE_VERSION}.{WIRE_MINOR}）", local.name, info.version),
    )
}

fn check_peer_path(peer: &ListEntry, path_map: &Result<HashMap<&str, &[PathInfo]>, Option<&str>>) -> CheckItem {
    let name = &peer.report.name;
    let check_name = format!("path:{name}");

    match path_map {
        Ok(map) => {
            if let Some(paths) = map.get(name.as_str()).filter(|p| !p.is_empty()) {
                return evaluate_detailed_paths(&check_name, name, paths);
            }
            evaluate_summary_path(&check_name, name, peer.report.path, peer.report.rtt_ms, None)
        }
        Err(reason) => evaluate_summary_path(&check_name, name, peer.report.path, peer.report.rtt_ms, *reason),
    }
}

fn evaluate_detailed_paths(check_name: &str, device: &str, paths: &[PathInfo]) -> CheckItem {
    let total = paths.len();
    let direct_count = paths.iter().filter(|p| p.kind == PathKind::Direct).count();
    let metered_count = paths.iter().filter(|p| p.metered).count();
    let unmetered_count = total - metered_count;
    let min_rtt_ms = paths.iter().map(|p| p.rtt_us).min().unwrap_or(0) as f64 / 1000.0;

    if direct_count > 0 && unmetered_count > 0 {
        let metered_note = if metered_count > 0 { format!("，含 {metered_count} 条计费路径") } else { String::new() };
        return CheckItem::ok(
            check_name,
            format!("直连（共 {total} 条路径，直连 {direct_count} 条{metered_note}，最低往返 {min_rtt_ms:.1} ms）"),
        );
    }
    if direct_count > 0 {
        return CheckItem::warn(
            check_name,
            format!("直连但全部 {total} 条路径均标记为计费（最低往返 {min_rtt_ms:.1} ms）"),
            "计费路径不承担批量传输；接入非计费网络或运行 nexus iface 调整计费标记",
        );
    }
    let metered_note = if metered_count > 0 { format!("，含 {metered_count} 条计费路径") } else { String::new() };
    CheckItem::warn(
        check_name,
        format!("仅经中继连接（{total} 条路径{metered_note}，最低往返 {min_rtt_ms:.1} ms）"),
        format!("运行 nexus path {device} 查看路径详情；若两端均在对称 NAT 后，可将锚点或中继部署在更近位置"),
    )
}

fn evaluate_summary_path(
    check_name: &str,
    device: &str,
    kind: Option<PathKind>,
    rtt_ms: Option<u64>,
    paths_err: Option<&str>,
) -> CheckItem {
    let rtt_note = rtt_ms.map(|ms| format!("，往返 {ms} ms")).unwrap_or_default();
    if let Some(err) = paths_err {
        let base = match kind {
            Some(PathKind::Direct) => format!("直连{rtt_note}，但无法读取多路径与计费明细（{err}）"),
            Some(PathKind::Relay) => format!("经中继连接{rtt_note}，且无法读取多路径与计费明细（{err}）"),
            None => format!("无法读取路径明细（{err}）"),
        };
        return CheckItem::warn(check_name, base, format!("运行 nexus path {device} 检查连接路径"));
    }
    match kind {
        Some(PathKind::Direct) => CheckItem::ok(check_name, format!("直连{rtt_note}")),
        Some(PathKind::Relay) => CheckItem::warn(
            check_name,
            format!("经中继连接{rtt_note}"),
            format!("运行 nexus path {device} 查看路径详情；若打洞失败可就近部署锚点或中继"),
        ),
        None => CheckItem::warn(
            check_name,
            "设备在线但未获取到路径类型信息",
            format!("运行 nexus status {device} 或 nexus path {device} 确认连接路径"),
        ),
    }
}

fn check_version(local_version: &str, online_peers: &[&ListEntry]) -> CheckItem {
    if online_peers.is_empty() {
        return CheckItem::warn(
            "version",
            format!("没有在线的对端设备，无法核对版本兼容性（本机 {local_version}，协议 {WIRE_VERSION}.{WIRE_MINOR}）"),
            "待其他设备上线后重新运行 nexus doctor",
        );
    }

    let local_parsed = parse_semver(local_version);
    let mut older = Vec::new();
    let mut newer = Vec::new();
    let mut other = Vec::new();

    for peer in online_peers {
        let name = peer.report.name.as_str();
        let Some(info) = &peer.report.info else {
            other.push(format!("{name}（未返回版本）"));
            continue;
        };
        if info.version == local_version {
            continue;
        }
        match (parse_semver(&info.version), local_parsed) {
            (Some(pv), Some(lv)) => match pv.cmp(&lv) {
                Ordering::Less => older.push((name, info.version.as_str())),
                Ordering::Greater => newer.push((name, info.version.as_str())),
                Ordering::Equal => other.push(format!("{name}({})", info.version)),
            },
            _ => other.push(format!("{name}({})", info.version)),
        }
    }

    if older.is_empty() && newer.is_empty() && other.is_empty() {
        return CheckItem::ok(
            "version",
            format!("所有在线设备版本一致（{local_version}，协议 {WIRE_VERSION}.{WIRE_MINOR}）"),
        );
    }

    let mut details = Vec::new();
    let mut suggestions = Vec::new();
    if !older.is_empty() {
        let list = older.iter().map(|(n, v)| format!("{n}({v})")).collect::<Vec<_>>().join("、");
        let names = older.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(",");
        details.push(format!("较旧设备：{list}"));
        suggestions.push(format!("升级较旧设备：nexus upgrade --to {names} --file <新版本>"));
    }
    if !newer.is_empty() {
        let list = newer.iter().map(|(n, v)| format!("{n}({v})")).collect::<Vec<_>>().join("、");
        details.push(format!("较新设备：{list}"));
        suggestions.push("升级本机以匹配较新设备：nexus upgrade --file <新版本>".to_string());
    }
    if !other.is_empty() {
        details.push(format!("版本差异：{}", other.join("、")));
        if suggestions.is_empty() {
            suggestions.push("统一本机与对端版本：nexus upgrade --file <新版本>".to_string());
        }
    }

    CheckItem::warn(
        "version",
        format!("本机 {local_version}（协议 {WIRE_VERSION}.{WIRE_MINOR}），{}", details.join("；")),
        suggestions.join("；"),
    )
}

fn check_name_resolution(
    tun: &Option<Result<Option<TunReport>, String>>,
    route_explain: &Option<Result<RouteExplanation, String>>,
    probe_device: &str,
) -> CheckItem {
    let probe = format!("probe.{probe_device}.internal:80");
    match route_explain {
        Some(Ok(RouteExplanation::Service { .. })) => match tun {
            Some(Ok(Some(t))) => CheckItem::ok(
                "name_resolution",
                format!("完整节点名字服务可用（DNS {}，网卡 {}，网段 {}），.internal 服务路由正常", t.dns, t.name, t.net),
            ),
            Some(Ok(None)) => CheckItem::ok(
                "name_resolution",
                ".internal 服务名字路由可用（未启用 --tun 完整节点内置 DNS）",
            ),
            Some(Err(err)) => CheckItem::warn(
                "name_resolution",
                format!(".internal 服务路由可用，但无法查询完整节点 DNS 状态（{err}）"),
                "运行 nexus status 检查完整节点虚拟网卡与 DNS 状态",
            ),
            None => CheckItem::warn(
                "name_resolution",
                ".internal 服务路由可用，但未获取到完整节点 DNS 状态",
                "运行 nexus status 检查完整节点虚拟网卡与 DNS 状态",
            ),
        },
        Some(Ok(_)) => CheckItem::fail(
            "name_resolution",
            format!(".internal 服务名字路由判定异常（{probe} 未解析为服务路由）"),
            format!("运行 nexus route ls 和 nexus route explain {probe} 检查路由规则"),
        ),
        Some(Err(err)) => CheckItem::warn(
            "name_resolution",
            format!("当前控制面无法验证 .internal 名字解析（{err}）"),
            format!("运行 nexus route explain {probe} 排查路由与名字解析"),
        ),
        None => CheckItem::warn(
            "name_resolution",
            "未获取到名字解析与路由诊断数据",
            format!("运行 nexus route explain {probe} 排查路由与名字解析"),
        ),
    }
}

fn parse_semver(s: &str) -> Option<(u64, u64, u64)> {
    let core = s.trim().trim_start_matches('v').split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

pub fn exit_code(report: &DoctorReport) -> i32 {
    if report.overall == CheckStatus::Fail { 1 } else { 0 }
}

pub fn format_text(report: &DoctorReport) -> String {
    let mut out = String::new();
    let mut ok = 0usize;
    let mut warn = 0usize;
    let mut fail = 0usize;

    for item in &report.checks {
        match item.status {
            CheckStatus::Ok => ok += 1,
            CheckStatus::Warn => warn += 1,
            CheckStatus::Fail => fail += 1,
        }
        out.push_str(&format!("[{:<4}] {:<18} {}\n", item.status.badge(), item.name, item.detail));
        if let Some(suggestion) = &item.suggestion {
            out.push_str(&format!("       建议：{suggestion}\n"));
        }
    }
    out.push_str(&format!("汇总：{}（通过 {ok}，警告 {warn}，失败 {fail}）\n", report.overall.as_str()));
    out
}

pub fn format_json(report: &DoctorReport) -> String {
    let checks: Vec<Value> = report
        .checks
        .iter()
        .map(|c| {
            json!({
                "name": c.name,
                "status": c.status.as_str(),
                "detail": c.detail,
                "suggestion": c.suggestion,
            })
        })
        .collect();
    doc(json!({
        "schema": JSON_SCHEMA,
        "overall": report.overall.as_str(),
        "checks": checks,
    }))
}

pub async fn run<C: Ctl, K: Sink>(ctl: &C, json_out: bool, sink: &mut K) -> i32 {
    let daemon = match call(ctl, CtlRequest::Status).await {
        Ok(CtlResponse::Status(st)) => Ok(st),
        Ok(CtlResponse::Error(e)) if e.code == ErrorCode::Denied => return fail_wire(sink, &e),
        Ok(CtlResponse::Error(e)) => Err(e.message),
        Ok(_) => Err("常驻程序的回应无法理解".to_string()),
        Err(e) => Err(e.to_string()),
    };

    let snap = match daemon {
        Err(err) => DoctorSnapshot {
            cli_version: env!("CARGO_PKG_VERSION").to_string(),
            daemon: Err(err),
            list: None,
            paths: None,
            tun: None,
            route_explain: None,
        },
        Ok(st) => {
            let list = match call(ctl, CtlRequest::List).await {
                Ok(CtlResponse::List(entries)) => Ok(entries),
                Ok(CtlResponse::Error(e)) => Err(e.message),
                Ok(_) => Err("常驻程序的回应无法理解".to_string()),
                Err(e) => Err(e.to_string()),
            };
            let paths = match call(ctl, CtlRequest::Paths { device: None }).await {
                Ok(CtlResponse::Paths(p)) => Ok(p),
                Ok(CtlResponse::Error(e)) if e.code == ErrorCode::InvalidState => Ok(Vec::new()),
                Ok(CtlResponse::Error(e)) => Err(e.message),
                Ok(_) => Err("常驻程序的回应无法理解".to_string()),
                Err(e) => Err(e.to_string()),
            };
            let tun = match call(ctl, CtlRequest::TunShow).await {
                Ok(CtlResponse::Tun(t)) => Ok(t),
                Ok(CtlResponse::Error(e)) => Err(e.message),
                Ok(_) => Err("常驻程序的回应无法理解".to_string()),
                Err(e) => Err(e.to_string()),
            };
            let probe_device = list
                .as_ref()
                .ok()
                .and_then(|entries| {
                    entries
                        .iter()
                        .filter(|e| !e.is_self)
                        .map(|e| e.report.name.as_str())
                        .min()
                })
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| if st.name.is_empty() { "self" } else { st.name.as_str() })
                .to_string();
            let route_explain = match call(ctl, CtlRequest::RouteExplain { target: format!("probe.{probe_device}.internal:80") }).await {
                Ok(CtlResponse::Explained(ex)) => Ok(ex),
                Ok(CtlResponse::Error(e)) => Err(e.message),
                Ok(_) => Err("常驻程序的回应无法理解".to_string()),
                Err(e) => Err(e.to_string()),
            };
            DoctorSnapshot {
                cli_version: env!("CARGO_PKG_VERSION").to_string(),
                daemon: Ok(st),
                list: Some(list),
                paths: Some(paths),
                tun: Some(tun),
                route_explain: Some(route_explain),
            }
        }
    };

    let report = evaluate(&snap);
    let rendered = if json_out { format_json(&report) } else { format_text(&report) };
    sink.out(rendered.as_bytes());
    exit_code(&report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_core_proto::roster::DeviceRole;
    use nexus_core_proto::{NodeId, NodeInfo, Timestamp};

    fn status_report(name: &str, version: Option<&str>, online: bool, path: Option<PathKind>, rtt_ms: Option<u64>, joined: bool) -> StatusReport {
        StatusReport {
            name: name.into(),
            node_id: NodeId([name.len() as u8; 32]),
            info: version.map(|v| NodeInfo {
                node_id: NodeId([name.len() as u8; 32]),
                name: name.into(),
                version: v.into(),
                started_at: Timestamp(1_000),
                now: Timestamp(5_000),
                roster_version: joined.then_some(2),
            }),
            online,
            rtt_ms,
            path,
            role: joined.then_some(DeviceRole::Member),
        }
    }

    fn healthy_snapshot() -> DoctorSnapshot {
        let local = status_report("laptop", Some("0.1.0"), true, None, None, true);
        let peer = status_report("home", Some("0.1.0"), true, Some(PathKind::Direct), Some(4), true);
        DoctorSnapshot {
            cli_version: "0.1.0".into(),
            daemon: Ok(local.clone()),
            list: Some(Ok(vec![
                ListEntry { report: local, is_self: true },
                ListEntry { report: peer, is_self: false },
            ])),
            paths: Some(Ok(vec![DevicePaths { device: "home".into(), paths: vec![] }])),
            tun: Some(Ok(None)),
            route_explain: Some(Ok(RouteExplanation::Service { device: "home".into(), service: "probe".into() })),
        }
    }

    #[test]
    fn healthy_network_reports_ok_and_exit_zero() {
        let report = evaluate(&healthy_snapshot());
        assert_eq!(report.overall, CheckStatus::Ok);
        assert_eq!(exit_code(&report), 0);
        assert!(report.checks.iter().all(|c| c.status == CheckStatus::Ok && c.suggestion.is_none()));

        let json: Value = serde_json::from_str(&format_json(&report)).unwrap();
        assert_eq!(json["schema"], JSON_SCHEMA);
        assert_eq!(json["overall"], "ok");
        assert_eq!(json["checks"].as_array().unwrap().len(), 5);

        let text = format_text(&report);
        assert!(text.contains("汇总：ok（通过 5，警告 0，失败 0）"), "{text}");
    }

    #[test]
    fn unreachable_daemon_fails_with_nonzero_exit() {
        let snap = DoctorSnapshot {
            cli_version: "0.1.0".into(),
            daemon: Err("connection refused".into()),
            list: None,
            paths: None,
            tun: None,
            route_explain: None,
        };
        let report = evaluate(&snap);
        assert_eq!(report.overall, CheckStatus::Fail);
        assert_eq!(exit_code(&report), 1);
        assert_eq!(report.checks.len(), 1);
        assert_eq!(report.checks[0].name, "daemon");
        assert_eq!(report.checks[0].status, CheckStatus::Fail);
        assert!(report.checks[0].suggestion.is_some());
    }

    #[test]
    fn warn_only_keeps_exit_code_zero() {
        let mut snap = healthy_snapshot();
        snap.cli_version = "0.2.0".into();
        let report = evaluate(&snap);
        assert_eq!(report.overall, CheckStatus::Warn);
        assert_eq!(exit_code(&report), 0);
        let daemon = report.checks.iter().find(|c| c.name == "daemon").unwrap();
        assert_eq!(daemon.status, CheckStatus::Warn);
        assert!(daemon.suggestion.is_some());
    }

    #[test]
    fn offline_peer_marks_device_fail_and_omits_path_check() {
        let mut snap = healthy_snapshot();
        let local = status_report("laptop", Some("0.1.0"), true, None, None, true);
        let home = status_report("home", Some("0.1.0"), true, Some(PathKind::Direct), Some(3), true);
        let office = status_report("office", None, false, None, None, true);
        snap.list = Some(Ok(vec![
            ListEntry { report: local, is_self: true },
            ListEntry { report: office, is_self: false },
            ListEntry { report: home, is_self: false },
        ]));

        let report = evaluate(&snap);
        assert_eq!(report.overall, CheckStatus::Fail);
        assert_eq!(exit_code(&report), 1);

        let office_item = report.checks.iter().find(|c| c.name == "device:office").unwrap();
        assert_eq!(office_item.status, CheckStatus::Fail);
        assert!(office_item.suggestion.as_deref().unwrap().contains("office"));
        assert!(report.checks.iter().all(|c| c.name != "path:office"));
        assert_eq!(report.checks.iter().find(|c| c.name == "path:home").unwrap().status, CheckStatus::Ok);
    }

    #[test]
    fn paths_handle_multipath_metered_and_relay_only() {
        let path = |kind: PathKind, rtt_us: u64, metered: bool| PathInfo {
            local: "10.0.0.1".into(),
            remote: "10.0.0.2:4433".into(),
            kind,
            rtt_us,
            tx_bytes: 100,
            rx_bytes: 100,
            lost_packets: 0,
            metered,
        };

        let mut mixed = healthy_snapshot();
        mixed.paths = Some(Ok(vec![DevicePaths {
            device: "home".into(),
            paths: vec![path(PathKind::Direct, 5_000, false), path(PathKind::Direct, 12_000, true)],
        }]));
        let r = evaluate(&mixed);
        let p = r.checks.iter().find(|c| c.name == "path:home").unwrap();
        assert_eq!(p.status, CheckStatus::Ok);
        assert!(p.detail.contains("共 2 条路径") && p.detail.contains("含 1 条计费路径"), "{}", p.detail);

        let mut all_metered = healthy_snapshot();
        all_metered.paths = Some(Ok(vec![DevicePaths {
            device: "home".into(),
            paths: vec![path(PathKind::Direct, 8_000, true)],
        }]));
        let r = evaluate(&all_metered);
        assert_eq!(r.overall, CheckStatus::Warn);
        assert_eq!(exit_code(&r), 0);
        assert_eq!(r.checks.iter().find(|c| c.name == "path:home").unwrap().status, CheckStatus::Warn);

        let mut relay_only = healthy_snapshot();
        relay_only.paths = Some(Ok(vec![DevicePaths {
            device: "home".into(),
            paths: vec![path(PathKind::Relay, 45_000, false)],
        }]));
        let r = evaluate(&relay_only);
        let p = r.checks.iter().find(|c| c.name == "path:home").unwrap();
        assert_eq!(p.status, CheckStatus::Warn);
        assert!(p.detail.contains("中继"), "{}", p.detail);
        assert!(p.suggestion.as_deref().unwrap().contains("nexus path home"));
    }

    #[test]
    fn version_check_suggests_upgrade_direction() {
        let mut older_snap = healthy_snapshot();
        let local = status_report("laptop", Some("0.2.0"), true, None, None, true);
        let old_peer = status_report("home", Some("0.1.0"), true, Some(PathKind::Direct), Some(4), true);
        older_snap.cli_version = "0.2.0".into();
        older_snap.daemon = Ok(local.clone());
        older_snap.list = Some(Ok(vec![
            ListEntry { report: local.clone(), is_self: true },
            ListEntry { report: old_peer, is_self: false },
        ]));
        let r = evaluate(&older_snap);
        let v = r.checks.iter().find(|c| c.name == "version").unwrap();
        assert_eq!(v.status, CheckStatus::Warn);
        assert!(v.suggestion.as_deref().unwrap().contains("nexus upgrade --to home"));

        let mut newer_snap = older_snap;
        let new_peer = status_report("home", Some("0.3.0"), true, Some(PathKind::Direct), Some(4), true);
        newer_snap.list = Some(Ok(vec![
            ListEntry { report: local, is_self: true },
            ListEntry { report: new_peer, is_self: false },
        ]));
        let r = evaluate(&newer_snap);
        let v = r.checks.iter().find(|c| c.name == "version").unwrap();
        assert_eq!(v.status, CheckStatus::Warn);
        assert!(v.suggestion.as_deref().unwrap().contains("升级本机"));
    }

    #[test]
    fn name_resolution_handles_tun_degraded_and_bad_route() {
        let mut with_tun = healthy_snapshot();
        with_tun.tun = Some(Ok(Some(TunReport {
            name: "nexus0".into(),
            net: "198.18.0.0/16".into(),
            dns: "198.18.0.2".into(),
        })));
        let r = evaluate(&with_tun);
        let dns = r.checks.iter().find(|c| c.name == "name_resolution").unwrap();
        assert_eq!(dns.status, CheckStatus::Ok);
        assert!(dns.detail.contains("198.18.0.2"));

        let mut degraded = healthy_snapshot();
        degraded.route_explain = Some(Err("unsupported".into()));
        let r = evaluate(&degraded);
        assert_eq!(r.overall, CheckStatus::Warn);
        assert_eq!(exit_code(&r), 0);
        assert_eq!(r.checks.iter().find(|c| c.name == "name_resolution").unwrap().status, CheckStatus::Warn);

        let mut broken = healthy_snapshot();
        broken.route_explain = Some(Ok(RouteExplanation::BadInternalName));
        let r = evaluate(&broken);
        assert_eq!(r.overall, CheckStatus::Fail);
        assert_eq!(exit_code(&r), 1);
        assert_eq!(r.checks.iter().find(|c| c.name == "name_resolution").unwrap().status, CheckStatus::Fail);
    }

    #[test]
    fn unjoined_node_warns_instead_of_failing() {
        let local = status_report("laptop", Some("0.1.0"), true, None, None, false);
        let snap = DoctorSnapshot {
            cli_version: "0.1.0".into(),
            daemon: Ok(local.clone()),
            list: Some(Ok(vec![ListEntry { report: local, is_self: true }])),
            paths: Some(Ok(vec![])),
            tun: Some(Ok(None)),
            route_explain: Some(Ok(RouteExplanation::Service { device: "laptop".into(), service: "probe".into() })),
        };
        let report = evaluate(&snap);
        assert_eq!(report.overall, CheckStatus::Warn);
        assert_eq!(exit_code(&report), 0);
        assert_eq!(report.checks.iter().find(|c| c.name == "devices").unwrap().status, CheckStatus::Warn);
    }
}
