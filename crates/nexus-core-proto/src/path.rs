//! 资源路径与能力名的模式匹配。
//!
//! 资源路径以 `/` 分隔，例如 `home`、`home/session/build`。资源模式中
//! `*` 匹配恰好一段，`**` 只能出现在结尾，匹配零段或多段。
//!
//! 能力名以 `.` 分隔，例如 `term.exec`、`term.session.attach`。能力模式中
//! 结尾的 `*` 匹配一段或多段，所以 `term.*` 匹配所有以 `term.` 开头的能力，`*` 匹配全部。

use core::fmt;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathError(pub String);

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 一段名字只允许字母、数字、`-`、`_`，长度 1 到 64。
pub fn valid_segment(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// 一个具体的资源路径。
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ResourcePath(String);

impl ResourcePath {
    pub fn parse(s: &str) -> Result<Self, PathError> {
        if s.split('/').all(valid_segment) {
            Ok(Self(s.to_string()))
        } else {
            Err(PathError(format!("非法的资源路径：{s}")))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn segments(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }
}

/// 资源模式。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourcePattern(String);

impl ResourcePattern {
    pub fn parse(s: &str) -> Result<Self, PathError> {
        let segs: Vec<&str> = s.split('/').collect();
        let last = segs.len() - 1;
        let ok = segs.iter().enumerate().all(|(i, seg)| match *seg {
            "*" => true,
            "**" => i == last,
            other => valid_segment(other),
        });
        if ok {
            Ok(Self(s.to_string()))
        } else {
            Err(PathError(format!("非法的资源模式：{s}")))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// 模式是否匹配这个路径。
    pub fn matches(&self, path: &ResourcePath) -> bool {
        let mut pat = self.0.split('/');
        let mut segs = path.segments();
        loop {
            match (pat.next(), segs.next()) {
                (Some("**"), _) => return true,
                (None, None) => return true,
                (Some(p), Some(s)) if p == "*" || p == s => continue,
                _ => return false,
            }
        }
    }

    /// 是否存在某个等于 `prefix` 或位于 `prefix` 之下的路径能被这个模式匹配。
    /// 用来决定一个主体能否「看到」某个节点或目录。
    pub fn may_match_within(&self, prefix: &ResourcePath) -> bool {
        let mut pat = self.0.split('/');
        for seg in prefix.segments() {
            match pat.next() {
                Some("**") => return true,
                Some(p) if p == "*" || p == seg => continue,
                // 模式已经用完（只能匹配更短的路径），或这一段对不上
                _ => return false,
            }
        }
        // prefix 走完了，模式剩下的部分总能用某个下级路径补齐
        true
    }
}

/// 一个具体的能力名。
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Capability(String);

impl Capability {
    pub fn parse(s: &str) -> Result<Self, PathError> {
        if s.split('.').all(valid_segment) {
            Ok(Self(s.to_string()))
        } else {
            Err(PathError(format!("非法的能力名：{s}")))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// 能力模式。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityPattern(String);

impl CapabilityPattern {
    pub fn parse(s: &str) -> Result<Self, PathError> {
        let segs: Vec<&str> = s.split('.').collect();
        let last = segs.len() - 1;
        let ok = segs
            .iter()
            .enumerate()
            .all(|(i, seg)| if *seg == "*" { i == last } else { valid_segment(seg) });
        if ok {
            Ok(Self(s.to_string()))
        } else {
            Err(PathError(format!("非法的能力模式：{s}")))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn matches(&self, cap: &Capability) -> bool {
        let mut pat = self.0.split('.');
        let mut segs = cap.0.split('.');
        loop {
            match (pat.next(), segs.next()) {
                // 结尾的 * 至少吃掉一段
                (Some("*"), Some(_)) => return true,
                (None, None) => return true,
                (Some(p), Some(s)) if p == s => continue,
                _ => return false,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> ResourcePath {
        ResourcePath::parse(s).unwrap()
    }
    fn rp(s: &str) -> ResourcePattern {
        ResourcePattern::parse(s).unwrap()
    }
    fn c(s: &str) -> Capability {
        Capability::parse(s).unwrap()
    }
    fn cp(s: &str) -> CapabilityPattern {
        CapabilityPattern::parse(s).unwrap()
    }

    #[test]
    fn parse_rejects_malformed_input() {
        for bad in ["", "/", "home/", "/home", "a//b", "a b", "ä", "*", "a/*"] {
            assert!(ResourcePath::parse(bad).is_err(), "路径 {bad:?} 应当非法");
        }
        for bad in ["", "a/**/b", "**/a", "a/***", "a b"] {
            assert!(ResourcePattern::parse(bad).is_err(), "资源模式 {bad:?} 应当非法");
        }
        for bad in ["", "term.", ".exec", "*.exec", "term.*.x", "term exec"] {
            assert!(CapabilityPattern::parse(bad).is_err(), "能力模式 {bad:?} 应当非法");
        }
        assert!(Capability::parse("term.*").is_err());
        assert!(ResourcePath::parse(&"a".repeat(65)).is_err());
        assert!(ResourcePath::parse(&"a".repeat(64)).is_ok());
    }

    #[test]
    fn names_may_contain_dash_and_underscore() {
        for good in ["my-laptop", "node_1", "a-b_c", "A9"] {
            assert!(valid_segment(good), "{good} 应当合法");
            assert!(ResourcePath::parse(good).is_ok());
        }
        assert_eq!(ResourcePath::parse("my-laptop/session_1").unwrap().as_str(), "my-laptop/session_1");
    }

    #[test]
    fn patterns_keep_their_text_and_errors_explain_themselves() {
        assert_eq!(rp("home/*/x/**").as_str(), "home/*/x/**");
        assert_eq!(cp("term.*").as_str(), "term.*");
        assert_eq!(c("term.exec").as_str(), "term.exec");
        let err = ResourcePattern::parse("a/**/b").unwrap_err();
        assert!(err.to_string().contains("a/**/b"), "{err}");
    }

    #[test]
    fn resource_pattern_examples() {
        // covers: R12
        assert!(rp("home").matches(&p("home")));
        assert!(!rp("home").matches(&p("home/session")));
        assert!(!rp("home").matches(&p("office")));
        assert!(rp("home/*").matches(&p("home/session")));
        assert!(!rp("home/*").matches(&p("home")));
        assert!(!rp("home/*").matches(&p("home/session/main")));
        assert!(rp("home/**").matches(&p("home")));
        assert!(rp("home/**").matches(&p("home/session/main")));
        assert!(!rp("home/**").matches(&p("homeoffice")));
        assert!(rp("*/session/*").matches(&p("office/session/x")));
        assert!(!rp("*/session/*").matches(&p("office/files/x")));
        assert!(rp("**").matches(&p("anything/at/all")));
    }

    #[test]
    fn capability_pattern_examples() {
        // covers: R12
        assert!(cp("term.exec").matches(&c("term.exec")));
        assert!(!cp("term.exec").matches(&c("term.exec.more")));
        assert!(!cp("term.exec").matches(&c("term")));
        assert!(cp("term.*").matches(&c("term.exec")));
        assert!(cp("term.*").matches(&c("term.session.attach")));
        assert!(!cp("term.*").matches(&c("term")));
        assert!(!cp("term.*").matches(&c("terminal.exec")));
        assert!(cp("*").matches(&c("node.info")));
    }

    #[test]
    fn visibility_examples() {
        // covers: R15
        let g = rp("home/session/build");
        assert!(g.may_match_within(&p("home")));
        assert!(g.may_match_within(&p("home/session")));
        assert!(g.may_match_within(&p("home/session/build")));
        assert!(!g.may_match_within(&p("home/session/other")));
        assert!(!g.may_match_within(&p("home/session/build/deeper")));
        assert!(!g.may_match_within(&p("office")));
        assert!(rp("*/session/*").may_match_within(&p("office")));
        assert!(rp("home/**").may_match_within(&p("home/a/b/c")));
    }
}
