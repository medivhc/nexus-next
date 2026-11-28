//! Android 应用分流（绕过 VPN）策略与防环路归一化（N1）。

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// 用户配置的应用绕过策略：列表中的包名经 OS 级 `addDisallowedApplication` 绕过 VPN。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppPolicy {
    pub bypass_packages: Vec<String>,
}

impl AppPolicy {
    /// 从按行/逗号分隔的文本解析绕过列表（忽略 `#` 注释与空行）。
    pub fn parse_lines(text: &str) -> Self {
        let bypass_packages = text
            .lines()
            .flat_map(|line| {
                let line = line.split('#').next().unwrap_or("");
                line.split(',')
            })
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();
        Self { bypass_packages }
    }

    /// 计算最终生效的 `disallowed_packages` 列表：
    /// 1. 过滤非法包名字符；
    /// 2. 去重并按字典序排序（确定性输出）；
    /// 3. 强制包含 `self_package`，杜绝 VPN 递归环路。
    pub fn effective_disallowed_packages(&self, self_package: &str) -> Vec<String> {
        let mut set = BTreeSet::new();
        for pkg in &self.bypass_packages {
            let trimmed = pkg.trim();
            if is_valid_package_name(trimmed) {
                set.insert(trimmed.to_string());
            }
        }
        let self_pkg = self_package.trim();
        if is_valid_package_name(self_pkg) {
            set.insert(self_pkg.to_string());
        }
        set.into_iter().collect()
    }
}

fn is_valid_package_name(s: &str) -> bool {
    !s.is_empty()
        && s.contains('.')
        && s.split('.').all(|seg| {
            !seg.is_empty()
                && seg
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        })
}
