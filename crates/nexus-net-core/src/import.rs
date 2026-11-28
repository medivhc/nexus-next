//! Clash / Mihomo 规则子集的可审计导入（N1）。
//!
//! 仅支持可静态审计、无隐式副作用的规则子集（`DOMAIN`、`DOMAIN-SUFFIX`、`IP-CIDR`、`IP-CIDR6`、`MATCH`）。
//! 不支持 `SCRIPT`、`MITM`、`RULE-SET`、`GEOIP`、`PROCESS-NAME` 等特性，逐条记录跳过原因，
//! 绝不静默启用新权限或把未映射的代理组降级为直连。

use crate::route::{Matcher, Outbound, Rule};
use serde::{Deserialize, Serialize};

/// 导入选项：指明非 DIRECT/REJECT 的策略组应映射到哪个 Nexus 出口或上游 SOCKS5。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportOptions {
    /// 当规则指向代理组时，优先映射到的 Nexus 出口设备名。
    pub default_exit: Option<String>,
    /// 未指定 `default_exit` 时，映射到的上游 SOCKS5 地址（`主机:端口`）。
    pub default_socks5: Option<String>,
}

/// 一条被跳过或不支持的规则记录。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedRule {
    pub line: usize,
    pub raw: String,
    pub reason: String,
}

/// Clash 规则子集导入审计报告。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportReport {
    pub imported: Vec<Rule>,
    pub skipped: Vec<SkippedRule>,
    pub notes: Vec<String>,
}

/// 解析 Clash / Mihomo YAML 中的 `rules:` 段落（或纯 `- TYPE,ARG,ACTION` 行）。
pub fn parse_clash_rules(input: &str, opts: &ImportOptions) -> ImportReport {
    let mut report = ImportReport::default();
    let has_rules_header = input
        .lines()
        .any(|l| l.trim_start() == "rules:" || l.trim_start().starts_with("rules: "));
    let mut in_rules = !has_rules_header;
    let mut saw_mitm_or_script = false;

    for (idx, raw_line) in input.lines().enumerate() {
        let line_no = idx + 1;
        let trimmed = raw_line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        // 顶层 YAML key 切换段落
        if !raw_line.starts_with(' ') && !raw_line.starts_with('\t') && !trimmed.starts_with('-') {
            let key = trimmed
                .split(':')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            if key == "rules" {
                in_rules = true;
                continue;
            }
            if key == "mitm" || key == "script" {
                saw_mitm_or_script = true;
            }
            in_rules = false;
            continue;
        }
        if !in_rules {
            continue;
        }

        let item = match trimmed.strip_prefix('-') {
            Some(rest) => rest.trim(),
            None => continue,
        };
        // 去掉行尾注释和包裹引号
        let item = strip_inline_comment(item).trim();
        let item = item
            .strip_prefix('\'')
            .and_then(|s| s.strip_suffix('\''))
            .or_else(|| item.strip_prefix('"').and_then(|s| s.strip_suffix('"')))
            .unwrap_or(item)
            .trim();
        if item.is_empty() {
            continue;
        }

        let parts: Vec<&str> = item.split(',').map(str::trim).collect();
        let kind = parts[0].to_ascii_uppercase();
        match kind.as_str() {
            "DOMAIN" | "DOMAIN-SUFFIX" | "IP-CIDR" | "IP-CIDR6" => {
                if parts.len() < 3 {
                    report.skipped.push(SkippedRule {
                        line: line_no,
                        raw: item.to_string(),
                        reason: "规则字段不完整，至少需要 TYPE,ARG,ACTION".into(),
                    });
                    continue;
                }
                let matcher = match kind.as_str() {
                    "DOMAIN" => Matcher::parse_domain_exact(parts[1]),
                    "DOMAIN-SUFFIX" => Matcher::parse_domain(parts[1]),
                    "IP-CIDR" | "IP-CIDR6" => Matcher::parse_cidr(parts[1]),
                    _ => unreachable!(),
                };
                let matcher = match matcher {
                    Ok(m) => m,
                    Err(e) => {
                        report.skipped.push(SkippedRule {
                            line: line_no,
                            raw: item.to_string(),
                            reason: e,
                        });
                        continue;
                    }
                };
                match map_action(parts[2], opts) {
                    Ok(outbound) => report.imported.push(Rule { matcher, outbound }),
                    Err(reason) => report.skipped.push(SkippedRule {
                        line: line_no,
                        raw: item.to_string(),
                        reason,
                    }),
                }
            }
            "MATCH" => {
                let policy = parts.get(1).copied().unwrap_or("");
                report.notes.push(format!(
                    "第 {line_no} 行 MATCH,{policy}：Nexus 默认动作保持显式直连，未静默覆盖；如需全量出口请显式配置规则"
                ));
            }
            "SCRIPT" | "GEOIP" | "RULE-SET" | "PROCESS-NAME" | "DOMAIN-KEYWORD" | "DST-PORT"
            | "SRC-IP-CIDR" => {
                report.skipped.push(SkippedRule {
                    line: line_no,
                    raw: item.to_string(),
                    reason: format!("不支持的规则类型 {kind}（首阶段仅支持可审计子集，不自动启用脚本或模糊匹配）"),
                });
            }
            _ => {
                report.skipped.push(SkippedRule {
                    line: line_no,
                    raw: item.to_string(),
                    reason: format!("未识别的规则类型 {kind}"),
                });
            }
        }
    }

    if saw_mitm_or_script {
        report
            .notes
            .push("检测到配置中包含 mitm 或 script 段落：已安全忽略，绝不自动启用".into());
    }
    report
}

fn strip_inline_comment(s: &str) -> &str {
    // 仅去掉引号外的 ` #` 注释
    let mut in_single = false;
    let mut in_double = false;
    let bytes = s.as_bytes();
    for i in 0..bytes.len() {
        match bytes[i] {
            b'\'' if !in_double => in_single = !in_single,
            b'"' if !in_single => in_double = !in_double,
            b'#' if !in_single && !in_double && (i == 0 || bytes[i - 1].is_ascii_whitespace()) => {
                return &s[..i];
            }
            _ => {}
        }
    }
    s
}

fn map_action(action: &str, opts: &ImportOptions) -> Result<Outbound, String> {
    match action.trim().to_ascii_uppercase().as_str() {
        "DIRECT" => Ok(Outbound::Direct),
        "REJECT" | "REJECT-DROP" => Ok(Outbound::Reject),
        "" => Err("缺少目标动作".into()),
        _ => {
            if let Some(exit) = opts
                .default_exit
                .as_deref()
                .filter(|s| !s.trim().is_empty())
            {
                Ok(Outbound::Exit(exit.trim().to_string()))
            } else if let Some(up) = opts
                .default_socks5
                .as_deref()
                .filter(|s| !s.trim().is_empty())
            {
                Ok(Outbound::Socks5(up.trim().to_string()))
            } else {
                Err(format!(
                    "代理组 {action} 未配置映射出口（请指定 --via 或 --socks5）；为防泄露已跳过，不降级直连"
                ))
            }
        }
    }
}
