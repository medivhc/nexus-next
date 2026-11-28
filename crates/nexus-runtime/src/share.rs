//! 临时分享（M9.1）：文件或目录、已发布的网页服务、终端的只读观看；
//! 带到期时间、次数限制、只读隔离，访客凭链接在网页上直接使用，随时撤销并记录审计。

use crate::drive::Cas;
use crate::{err, internal, load, save, Dispatch, Node, DEFAULT_WEB_PAGE};
use nexus_core_proto::authz::{AuditEvent, DenyReason, Outcome, PrincipalRef};
use nexus_core_proto::{NodeId, Timestamp};
use nexus_drive_core::tree::{self, Entry, Store};
use nexus_drive_core::FileManifest;
use nexus_fabric::Transport;
use nexus_sys::System;
use nexus_wire::{
    hex_encode, Response, ShareCreatedReport, ShareDriveEntry, ShareInfo, ShareRecord, ShareTarget,
    Size, WireError,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub(crate) const KEY_SHARES: &str = "shares-v1";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct StoredShare {
    pub id: String,
    pub token: [u8; 16],
    pub target: ShareTarget,
    pub created_at: Timestamp,
    pub expires_at: Timestamp,
    pub max_uses: Option<u32>,
    pub uses: u32,
    pub revoked: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct ShareStore {
    pub entries: BTreeMap<String, StoredShare>,
}

impl ShareStore {
    pub fn load<S: System>(sys: &S) -> std::io::Result<Self> {
        Ok(load(sys, KEY_SHARES)?.unwrap_or_default())
    }
}

fn norm_parts(path: &str) -> Result<Vec<String>, WireError> {
    let parts: Vec<String> = path
        .split('/')
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect();
    if let Some(bad) = parts
        .iter()
        .find(|p| !nexus_drive_core::path::valid_part(p))
    {
        return Err(err(ErrorCode::InvalidInput, format!("路径里不能有 {bad}")));
    }
    Ok(parts)
}

use nexus_wire::ErrorCode;

impl<T: Transport, S: System> Node<T, S> {
    fn save_shares(&self) -> Result<(), WireError> {
        save(&self.system, KEY_SHARES, &*self.shares.borrow()).map_err(internal)
    }

    fn audit_share(
        &self,
        peer: NodeId,
        id: &str,
        capability: &str,
        resource: &str,
        outcome: Outcome,
    ) {
        if let Some(roster) = self.roster() {
            self.audit(
                &roster,
                &AuditEvent {
                    at: self.system.now(),
                    peer,
                    principal: PrincipalRef::Web {
                        label: format!("share:{id}"),
                    },
                    capability: capability.to_string(),
                    resource: resource.to_string(),
                    outcome,
                },
            );
        }
    }

    pub(crate) fn share_create(
        &self,
        target: ShareTarget,
        ttl_ms: u64,
        max_uses: Option<u32>,
        page: Option<String>,
    ) -> Result<ShareCreatedReport, WireError> {
        let roster = self
            .roster()
            .ok_or_else(|| err(ErrorCode::InvalidState, "本机还没有加入任何网络"))?;
        if ttl_ms == 0 {
            return Err(err(ErrorCode::InvalidInput, "--ttl 必须大于 0"));
        }
        if matches!(max_uses, Some(0)) {
            return Err(err(ErrorCode::InvalidInput, "--max-uses 必须大于 0"));
        }
        // 校验分享的目标在本机确实存在且符合类型约束。
        let normalized_target = match target {
            ShareTarget::Drive { ws, path } => {
                if !nexus_drive_core::path::valid_workspace(&ws) {
                    return Err(err(
                        ErrorCode::InvalidInput,
                        format!("工作区名不合法：{ws}"),
                    ));
                }
                let parts = norm_parts(&path)?;
                let ws_state = self
                    .drive_state
                    .borrow()
                    .workspaces
                    .get(&ws)
                    .cloned()
                    .ok_or_else(|| err(ErrorCode::NotFound, format!("本机上没有工作区 {ws}")))?;
                let cas = Cas::new(&self.system);
                match tree::lookup(&cas, ws_state.work, &parts)
                    .map_err(|e| internal(format!("{e:?}")))?
                {
                    Some(_) => {}
                    None => {
                        return Err(err(
                            ErrorCode::NotFound,
                            format!("{ws}:/{} 不存在", parts.join("/")),
                        ))
                    }
                }
                ShareTarget::Drive {
                    ws,
                    path: format!("/{}", parts.join("/")),
                }
            }
            ShareTarget::Service { name } => {
                let svcs = self.services.borrow();
                let svc = svcs
                    .iter()
                    .find(|s| s.name == name)
                    .ok_or_else(|| err(ErrorCode::NotFound, format!("本机没有发布服务 {name}")))?;
                if svc.proto != "http" {
                    return Err(err(
                        ErrorCode::InvalidInput,
                        format!(
                            "服务 {name} 的协议是 {}，只能临时分享 --proto http 的网页服务",
                            svc.proto
                        ),
                    ));
                }
                ShareTarget::Service { name }
            }
            ShareTarget::Terminal { session } => {
                if !self.sessions.borrow().contains_key(&session) {
                    return Err(err(
                        ErrorCode::NotFound,
                        format!("本机没有正在运行的终端会话 {session}"),
                    ));
                }
                ShareTarget::Terminal { session }
            }
        };

        let mut token = [0u8; 16];
        self.system.random(&mut token);
        let id = hex_encode(&token[..4]);
        let now = self.system.now();
        let expires_at = Timestamp(now.0.saturating_add(ttl_ms));
        let stored = StoredShare {
            id: id.clone(),
            token,
            target: normalized_target.clone(),
            created_at: now,
            expires_at,
            max_uses,
            uses: 0,
            revoked: false,
        };
        self.shares.borrow_mut().entries.insert(id.clone(), stored);
        self.save_shares()?;

        let base_page = page
            .or(roster.anchor)
            .unwrap_or_else(|| DEFAULT_WEB_PAGE.to_string());
        let url = format!(
            "{}/#share={}&node={}&hint={}",
            base_page.trim_end_matches('/'),
            hex_encode(&token),
            self.me(),
            self.transport.local_hint()
        );
        Ok(ShareCreatedReport {
            id,
            url,
            target: normalized_target,
            expires_at,
            max_uses,
        })
    }

    pub(crate) fn share_list(&self) -> Result<Vec<ShareRecord>, WireError> {
        let store = self.shares.borrow();
        Ok(store
            .entries
            .values()
            .map(|s| ShareRecord {
                id: s.id.clone(),
                target: s.target.clone(),
                created_at: s.created_at,
                expires_at: s.expires_at,
                max_uses: s.max_uses,
                uses: s.uses,
                revoked: s.revoked,
            })
            .collect())
    }

    pub(crate) fn share_revoke(&self, id: &str) -> Result<ShareRecord, WireError> {
        let record = {
            let mut store = self.shares.borrow_mut();
            let s = store
                .entries
                .get_mut(id)
                .ok_or_else(|| err(ErrorCode::NotFound, format!("没有编号为 {id} 的分享")))?;
            s.revoked = true;
            ShareRecord {
                id: s.id.clone(),
                target: s.target.clone(),
                created_at: s.created_at,
                expires_at: s.expires_at,
                max_uses: s.max_uses,
                uses: s.uses,
                revoked: s.revoked,
            }
        };
        self.save_shares()?;
        // 触发名册纪元递增，让正在观看该分享的只读终端流立即重新检查并断开。
        let next = *self.roster_epoch.borrow() + 1;
        let _ = self.roster_epoch.send(next);
        Ok(record)
    }

    /// 校验分享令牌是否仍然有效（`consume = true` 时在 `ShareOpen` 扣减一次次数）。
    fn validate_share(
        &self,
        peer: NodeId,
        token: &[u8; 16],
        capability: &str,
        consume: bool,
    ) -> Result<StoredShare, WireError> {
        let now = self.system.now();
        let me_name = self
            .roster()
            .and_then(|r| r.device(self.me()).map(|d| d.name.clone()))
            .unwrap_or_else(|| self.configured_name.clone());
        let mut store = self.shares.borrow_mut();
        let Some(entry) = store.entries.values_mut().find(|s| &s.token == token) else {
            drop(store);
            self.audit_share(
                peer,
                "unknown",
                capability,
                &me_name,
                Outcome::Deny(DenyReason::NoMatchingGrant),
            );
            return Err(err(ErrorCode::Denied, "分享链接无效"));
        };
        let id = entry.id.clone();
        let res = entry.target.describe(&me_name);
        if entry.revoked {
            drop(store);
            self.audit_share(
                peer,
                &id,
                capability,
                &res,
                Outcome::Deny(DenyReason::WebKeyRevoked),
            );
            return Err(err(ErrorCode::Denied, "这项分享已被撤销"));
        }
        if now >= entry.expires_at {
            drop(store);
            self.audit_share(
                peer,
                &id,
                capability,
                &res,
                Outcome::Deny(DenyReason::SessionExpired),
            );
            return Err(err(ErrorCode::Denied, "这项分享已过期"));
        }
        if let Some(max) = entry.max_uses {
            if consume && entry.uses >= max {
                drop(store);
                self.audit_share(
                    peer,
                    &id,
                    capability,
                    &res,
                    Outcome::Deny(DenyReason::NoMatchingGrant),
                );
                return Err(err(ErrorCode::Denied, "这项分享的使用次数已用完"));
            }
            if !consume && entry.uses > max {
                drop(store);
                self.audit_share(
                    peer,
                    &id,
                    capability,
                    &res,
                    Outcome::Deny(DenyReason::NoMatchingGrant),
                );
                return Err(err(ErrorCode::Denied, "这项分享的使用次数已用完"));
            }
        }
        if consume {
            entry.uses = entry.uses.saturating_add(1);
        }
        let snapshot = entry.clone();
        drop(store);
        if consume {
            let _ = self.save_shares();
        }
        self.audit_share(peer, &id, capability, &res, Outcome::Allow);
        Ok(snapshot)
    }

    pub(crate) fn share_is_active(&self, token: &[u8; 16]) -> bool {
        let now = self.system.now();
        let store = self.shares.borrow();
        store
            .entries
            .values()
            .any(|s| &s.token == token && !s.revoked && now < s.expires_at)
    }

    pub(crate) fn handle_share_open(&self, peer: NodeId, token: [u8; 16]) -> Response {
        let me_name = self
            .roster()
            .and_then(|r| r.device(self.me()).map(|d| d.name.clone()))
            .unwrap_or_else(|| self.configured_name.clone());
        match self.validate_share(peer, &token, "share.open", true) {
            Ok(s) => Response::ShareOpened(ShareInfo {
                id: s.id,
                device: me_name,
                target: s.target,
                expires_at: s.expires_at,
                remaining_uses: s.max_uses.map(|m| m.saturating_sub(s.uses)),
            }),
            Err(e) => Response::Error(e),
        }
    }

    pub(crate) fn handle_share_drive_list(
        &self,
        peer: NodeId,
        token: [u8; 16],
        sub_path: &str,
    ) -> Response {
        let result = (|| {
            let s = self.validate_share(peer, &token, "share.drive", false)?;
            let ShareTarget::Drive {
                ws,
                path: base_path,
            } = &s.target
            else {
                return Err(err(ErrorCode::InvalidInput, "这项分享不是文件或目录"));
            };
            let mut full_parts = norm_parts(base_path)?;
            let sub_parts = norm_parts(sub_path)?;
            let ws_state = self
                .drive_state
                .borrow()
                .workspaces
                .get(ws)
                .cloned()
                .ok_or_else(|| err(ErrorCode::NotFound, format!("工作区 {ws} 不存在")))?;
            let cas = Cas::new(&self.system);
            let base_entry = tree::lookup(&cas, ws_state.work, &full_parts)
                .map_err(|e| internal(format!("{e:?}")))?
                .ok_or_else(|| err(ErrorCode::NotFound, "分享的路径已不存在"))?;
            match base_entry {
                Entry::File { size, .. } => {
                    if !sub_parts.is_empty() {
                        return Err(err(ErrorCode::Denied, "不能越界访问单文件分享之外的路径"));
                    }
                    let name = full_parts.last().cloned().unwrap_or_else(|| ws.clone());
                    Ok(vec![ShareDriveEntry {
                        name,
                        kind: "file".into(),
                        size,
                        target: None,
                    }])
                }
                Entry::Symlink { target } => {
                    if !sub_parts.is_empty() {
                        return Err(err(ErrorCode::Denied, "不能越界访问链接分享之外的路径"));
                    }
                    let name = full_parts.last().cloned().unwrap_or_else(|| ws.clone());
                    Ok(vec![ShareDriveEntry {
                        name,
                        kind: "symlink".into(),
                        size: 0,
                        target: Some(target),
                    }])
                }
                Entry::Dir { .. } => {
                    full_parts.extend(sub_parts);
                    let target_entry = tree::lookup(&cas, ws_state.work, &full_parts)
                        .map_err(|e| internal(format!("{e:?}")))?
                        .ok_or_else(|| err(ErrorCode::NotFound, "目录不存在"))?;
                    let Entry::Dir { tree } = target_entry else {
                        return Err(err(ErrorCode::InvalidInput, "目标不是目录"));
                    };
                    let loaded = tree::load(&cas, tree).map_err(|e| internal(format!("{e:?}")))?;
                    let mut out: Vec<ShareDriveEntry> = loaded
                        .entries
                        .into_iter()
                        .map(|(name, e)| match e {
                            Entry::File { size, .. } => ShareDriveEntry {
                                name,
                                kind: "file".into(),
                                size,
                                target: None,
                            },
                            Entry::Dir { .. } => ShareDriveEntry {
                                name,
                                kind: "dir".into(),
                                size: 0,
                                target: None,
                            },
                            Entry::Symlink { target } => ShareDriveEntry {
                                name,
                                kind: "symlink".into(),
                                size: 0,
                                target: Some(target),
                            },
                        })
                        .collect();
                    out.sort_by(|a, b| (a.kind != "dir", &a.name).cmp(&(b.kind != "dir", &b.name)));
                    Ok(out)
                }
            }
        })();
        match result {
            Ok(entries) => Response::ShareDriveEntries(entries),
            Err(e) => Response::Error(e),
        }
    }

    pub(crate) fn handle_share_drive_read(
        &self,
        peer: NodeId,
        token: [u8; 16],
        sub_path: &str,
        max: u64,
    ) -> Response {
        let result = (|| {
            let s = self.validate_share(peer, &token, "share.drive", false)?;
            let ShareTarget::Drive {
                ws,
                path: base_path,
            } = &s.target
            else {
                return Err(err(ErrorCode::InvalidInput, "这项分享不是文件或目录"));
            };
            let mut full_parts = norm_parts(base_path)?;
            let sub_parts = norm_parts(sub_path)?;
            let ws_state = self
                .drive_state
                .borrow()
                .workspaces
                .get(ws)
                .cloned()
                .ok_or_else(|| err(ErrorCode::NotFound, format!("工作区 {ws} 不存在")))?;
            let cas = Cas::new(&self.system);
            let base_entry = tree::lookup(&cas, ws_state.work, &full_parts)
                .map_err(|e| internal(format!("{e:?}")))?
                .ok_or_else(|| err(ErrorCode::NotFound, "分享的路径已不存在"))?;
            let target_entry = match base_entry {
                Entry::File { .. } => {
                    if !sub_parts.is_empty()
                        && !(sub_parts.len() == 1 && full_parts.last() == sub_parts.first())
                    {
                        let me_name = self
                            .roster()
                            .and_then(|r| r.device(self.me()).map(|d| d.name.clone()))
                            .unwrap_or_default();
                        self.audit_share(
                            peer,
                            &s.id,
                            "share.drive",
                            &s.target.describe(&me_name),
                            Outcome::Deny(DenyReason::NoMatchingGrant),
                        );
                        return Err(err(ErrorCode::Denied, "不能越界读取被分享文件之外的路径"));
                    }
                    base_entry
                }
                Entry::Dir { .. } => {
                    if sub_parts.is_empty() {
                        return Err(err(
                            ErrorCode::InvalidInput,
                            "目标是目录，请指定要读取的文件子路径",
                        ));
                    }
                    full_parts.extend(sub_parts);
                    tree::lookup(&cas, ws_state.work, &full_parts)
                        .map_err(|e| internal(format!("{e:?}")))?
                        .ok_or_else(|| err(ErrorCode::NotFound, "文件不存在"))?
                }
                Entry::Symlink { target } => {
                    return Err(err(
                        ErrorCode::InvalidInput,
                        format!("目标是链接（指向 {target}）"),
                    ));
                }
            };
            let Entry::File { manifest, .. } = target_entry else {
                return Err(err(ErrorCode::InvalidInput, "目标是目录，不是文件"));
            };
            let m = cas
                .get(&manifest)
                .and_then(|b| FileManifest::decode(&b))
                .ok_or_else(|| internal("本机缺少文件清单"))?;
            let spans = m.spans(0, max);
            let mut bytes = Vec::new();
            for span in spans {
                let chunk = cas
                    .get(&span.hash)
                    .ok_or_else(|| internal("本机缺少文件数据块"))?;
                let part = chunk
                    .get(span.from as usize..span.to as usize)
                    .ok_or_else(|| internal("文件块范围越界"))?;
                bytes.extend_from_slice(part);
            }
            let truncated = (bytes.len() as u64) < m.size;
            Ok((m.size, bytes, truncated))
        })();
        match result {
            Ok((size, bytes, truncated)) => Response::ShareDriveFile {
                size,
                bytes,
                truncated,
            },
            Err(e) => Response::Error(e),
        }
    }

    pub(crate) fn handle_share_service(&self, peer: NodeId, token: [u8; 16]) -> Dispatch {
        let s = match self.validate_share(peer, &token, "share.service", false) {
            Ok(s) => s,
            Err(e) => return Dispatch::Reply(Response::Error(e)),
        };
        let ShareTarget::Service { name } = s.target else {
            return Dispatch::Reply(Response::Error(err(
                ErrorCode::InvalidInput,
                "这项分享不是网页服务",
            )));
        };
        match self.service_addr(&name) {
            Some(addr) => Dispatch::Pipe { addr, name },
            None => Dispatch::Reply(Response::Error(err(
                ErrorCode::NotFound,
                format!("服务 {name} 已取消发布"),
            ))),
        }
    }

    pub(crate) fn handle_share_terminal(
        &self,
        peer: NodeId,
        token: [u8; 16],
        size: Size,
    ) -> Dispatch {
        let s = match self.validate_share(peer, &token, "share.terminal", false) {
            Ok(s) => s,
            Err(e) => return Dispatch::Reply(Response::Error(e)),
        };
        let ShareTarget::Terminal { session } = s.target else {
            return Dispatch::Reply(Response::Error(err(
                ErrorCode::InvalidInput,
                "这项分享不是终端会话",
            )));
        };
        Dispatch::ShareSession {
            name: session,
            size,
            token,
            expires_at: s.expires_at,
        }
    }
}
