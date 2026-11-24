//! Vault（M9.2，R74）：用户秘密加密后同步到被授权的设备，按条目授权，每次取用有记录；
//! 吊销设备时列出要轮换的条目。

use crate::{err, internal, load, save, valid_segment, Node};
use nexus_core_proto::authz::{Action, Assertion, AuditEvent, Context, DenyReason, Outcome, Principal, PrincipalRef};
use nexus_core_proto::SecretKey;
use nexus_core_proto::path::{Capability, ResourcePath};
use nexus_core_proto::roster::DeviceRole;
use nexus_core_proto::vault as crypto_vault;
use nexus_core_proto::NodeId;
use nexus_fabric::Transport;
use nexus_sys::System;
use nexus_wire::{
    AgentCredential, ErrorCode, Request, Response, VaultCipherEntry, VaultEntryInfo, VaultRotateItem, WireError,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) const KEY_VAULT: &str = "vault-v1";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct VaultStore {
    /// 当前保存在本机的密文条目。管理设备保存全量条目；成员设备只保存授权给自己的条目。
    pub entries: BTreeMap<String, VaultCipherEntry>,
    /// 条目名 → 在当前版本下接触过该条目的设备名集合（用于设备被吊销时生成待轮换清单，重新 put 后重置）。
    pub exposed_to: BTreeMap<String, BTreeSet<String>>,
    /// 已吊销设备名 → 吊销时尚未轮换的条目列表。
    pub rotate_needed: BTreeMap<String, BTreeSet<String>>,
}

impl VaultStore {
    pub fn load<S: System>(sys: &S) -> std::io::Result<Self> {
        Ok(load(sys, KEY_VAULT)?.unwrap_or_default())
    }
}

impl<T: Transport, S: System> Node<T, S> {
    fn save_vault(&self) -> Result<(), WireError> {
        save(&self.system, KEY_VAULT, &*self.vault.borrow()).map_err(internal)
    }

    fn me_name(&self) -> String {
        self.roster()
            .and_then(|r| r.device(self.me()).map(|d| d.name.clone()))
            .unwrap_or_else(|| self.configured_name.clone())
    }

    /// 记录一次 Vault 取用审计（包括本机用户、Agent 或远端设备拉取）。
    fn audit_vault(&self, peer: NodeId, principal: PrincipalRef, name: &str, outcome: Outcome) {
        if let Some(roster) = self.roster() {
            let me_name = self.me_name();
            self.audit(
                &roster,
                &AuditEvent {
                    at: self.system.now(),
                    peer,
                    principal,
                    capability: "vault.read".into(),
                    resource: format!("{me_name}/vault/{name}"),
                    outcome,
                },
            );
        }
    }

    /// 校验本机调用方（用户或 Agent）是否有权读取条目 `name`，返回审计用的 `PrincipalRef`。
    fn check_local_vault_read(&self, name: &str, agent: Option<&AgentCredential>, record_audit: bool) -> Result<PrincipalRef, WireError> {
        let roster = self.roster().ok_or_else(|| err(ErrorCode::InvalidState, "本机还没有加入任何网络"))?;
        let me_name = self.me_name();
        let Some(agent) = agent else {
            let p = PrincipalRef::Device(self.me());
            if record_audit {
                self.audit_vault(self.me(), p.clone(), name, Outcome::Allow);
            }
            return Ok(p);
        };
        let action = Action {
            capability: Capability::parse("vault.read").expect("内置能力名合法"),
            resource: ResourcePath::parse(&format!("{me_name}/vault/{name}"))
                .map_err(|_| err(ErrorCode::InvalidInput, format!("条目名不合法：{name}")))?,
        };
        let mut nonce = [0u8; 16];
        self.system.random(&mut nonce);
        let now = self.system.now();
        let assertion = Assertion { target: self.me(), action: action.clone(), issued_at: now, nonce };
        let signature = assertion.sign(&SecretKey::from_seed(agent.secret));
        let principal = Principal::Agent { cert: agent.cert.clone(), assertion, signature };
        let verdict = nexus_core_proto::authz::decide(&Context { roster: &roster, me: self.me(), now }, self.me(), &principal, &action);
        if record_audit {
            self.audit(&roster, &verdict.audit);
        }
        match verdict.outcome {
            Outcome::Allow => Ok(PrincipalRef::Agent { id: agent.cert.cert.agent, label: agent.cert.cert.label.clone() }),
            Outcome::Deny(r) => Err(crate::denied(&r)),
        }
    }

    /// 在管理设备上写入或更新（轮换）一条秘密，并推送给相关设备。
    pub(crate) async fn vault_put(
        &self,
        name: String,
        value: Vec<u8>,
        devices: Vec<String>,
        agent: Option<&AgentCredential>,
    ) -> Result<VaultEntryInfo, WireError> {
        if agent.is_some() {
            return Err(err(ErrorCode::Denied, "agent 不能写入或修改 Vault 条目"));
        }
        let _root = self.admin_key()?;
        let roster = self.roster().ok_or_else(|| err(ErrorCode::InvalidState, "本机没有名册"))?;
        if !valid_segment(&name) {
            return Err(err(ErrorCode::InvalidInput, format!("条目名 {name:?} 不合法：只能用字母、数字、- 和 _")));
        }
        let me_name = self.me_name();
        let mut authorized: BTreeSet<String> = BTreeSet::new();
        authorized.insert(me_name.clone());
        for d in devices {
            if roster.device_by_name(&d).is_none() {
                return Err(err(ErrorCode::UnknownNode, format!("名册里没有设备 {d}")));
            }
            authorized.insert(d);
        }
        let authorized_vec: Vec<String> = authorized.iter().cloned().collect();

        let mut salt = [0u8; 16];
        self.system.random(&mut salt);
        let sealed = crypto_vault::seal(&roster.root, &name, salt, &value);
        let now = self.system.now();

        let (entry, prev_devices) = {
            let mut store = self.vault.borrow_mut();
            let prev = store.entries.get(&name);
            let version = prev.map_or(1, |e| e.version + 1);
            let prev_devs: Vec<String> = prev.map(|e| e.devices.clone()).unwrap_or_default();
            let entry = VaultCipherEntry {
                name: name.clone(),
                version,
                updated_at: now,
                devices: authorized_vec.clone(),
                sealed,
            };
            store.entries.insert(name.clone(), entry.clone());
            // 新写入/轮换后，接触过该当前版本秘密的设备重置为本次授权列表，并从待轮换清单中清除该条目。
            store.exposed_to.insert(name.clone(), authorized.clone());
            for set in store.rotate_needed.values_mut() {
                set.remove(&name);
            }
            store.rotate_needed.retain(|_, set| !set.is_empty());
            (entry, prev_devs)
        };
        self.save_vault()?;

        // 向本次授权的设备推送密文条目；向被移出授权的旧设备推送 None 以擦除其本地密文。
        for dev in roster.devices.iter().filter(|d| d.node_id != self.me()) {
            if authorized.contains(&dev.name) {
                let _ = self
                    .request(dev, Request::VaultPush { name: name.clone(), entry: Some(entry.clone()) }, None)
                    .await;
            } else if prev_devices.contains(&dev.name) {
                let _ = self
                    .request(dev, Request::VaultPush { name: name.clone(), entry: None }, None)
                    .await;
            }
        }

        Ok(VaultEntryInfo {
            name: entry.name,
            version: entry.version,
            updated_at: entry.updated_at,
            devices: entry.devices,
        })
    }

    /// 读取并解密一条秘密；每次调用都记入审计日志。
    pub(crate) fn vault_get(&self, name: &str, agent: Option<&AgentCredential>) -> Result<Vec<u8>, WireError> {
        if !valid_segment(name) {
            return Err(err(ErrorCode::InvalidInput, format!("条目名 {name:?} 不合法")));
        }
        let roster = self.roster().ok_or_else(|| err(ErrorCode::InvalidState, "本机还没有加入任何网络"))?;
        let me_name = self.me_name();
        let entry = {
            let store = self.vault.borrow();
            store.entries.get(name).cloned()
        };
        // 先做授权判定并记审计（无论允许或拒绝都会记录一条 vault.read 审计）。
        let principal_ref = self.check_local_vault_read(name, agent, true)?;
        let Some(entry) = entry else {
            return Err(err(ErrorCode::NotFound, format!("本机没有被授权的 Vault 条目 {name}")));
        };
        if !entry.devices.iter().any(|d| d == &me_name) {
            self.audit_vault(self.me(), principal_ref, name, Outcome::Deny(DenyReason::NoMatchingGrant));
            return Err(err(ErrorCode::Denied, format!("设备 {me_name} 没有被授权访问条目 {name}")));
        }
        let plain = crypto_vault::open(&roster.root, name, &entry.sealed)
            .ok_or_else(|| internal(format!("Vault 条目 {name} 密文校验失败")))?;
        Ok(plain)
    }

    /// 列出本机可见的 Vault 条目（Agent 只看到自己有 `vault.read` 授权的条目）。
    pub(crate) fn vault_list(&self, agent: Option<&AgentCredential>) -> Result<Vec<VaultEntryInfo>, WireError> {
        let me_name = self.me_name();
        let store = self.vault.borrow();
        let mut out = Vec::new();
        for e in store.entries.values() {
            if !e.devices.iter().any(|d| d == &me_name) {
                continue;
            }
            if agent.is_some() && self.check_local_vault_read(&e.name, agent, false).is_err() {
                continue;
            }
            out.push(VaultEntryInfo {
                name: e.name.clone(),
                version: e.version,
                updated_at: e.updated_at,
                devices: e.devices.clone(),
            });
        }
        Ok(out)
    }

    /// 在管理设备上删除一条秘密，并通知曾持有它的设备擦除。
    pub(crate) async fn vault_rm(&self, name: &str, agent: Option<&AgentCredential>) -> Result<(), WireError> {
        if agent.is_some() {
            return Err(err(ErrorCode::Denied, "agent 不能删除 Vault 条目"));
        }
        let _root = self.admin_key()?;
        let roster = self.roster().ok_or_else(|| err(ErrorCode::InvalidState, "本机没有名册"))?;
        let removed = {
            let mut store = self.vault.borrow_mut();
            let old = store
                .entries
                .remove(name)
                .ok_or_else(|| err(ErrorCode::NotFound, format!("没有 Vault 条目 {name}")))?;
            store.exposed_to.remove(name);
            for set in store.rotate_needed.values_mut() {
                set.remove(name);
            }
            store.rotate_needed.retain(|_, set| !set.is_empty());
            old
        };
        self.save_vault()?;
        for dev in roster.devices.iter().filter(|d| d.node_id != self.me() && removed.devices.contains(&d.name)) {
            let _ = self
                .request(dev, Request::VaultPush { name: name.to_string(), entry: None }, None)
                .await;
        }
        Ok(())
    }

    /// 从管理设备主动同步本机被授权的所有 Vault 条目。
    pub(crate) async fn vault_sync(&self) -> Result<Vec<VaultEntryInfo>, WireError> {
        let roster = self.roster().ok_or_else(|| err(ErrorCode::InvalidState, "本机还没有加入任何网络"))?;
        let me_name = self.me_name();
        let Some(admin) = roster.devices.iter().find(|d| d.role == DeviceRole::Admin).cloned() else {
            return Err(err(ErrorCode::InvalidState, "名册里没有管理设备"));
        };
        if admin.node_id == self.me() {
            return self.vault_list(None);
        }
        match self.request(&admin, Request::VaultPull, None).await {
            Ok((Response::VaultEntries(entries), _)) => {
                {
                    let mut store = self.vault.borrow_mut();
                    store.entries.clear();
                    for e in entries {
                        if e.devices.iter().any(|d| d == &me_name) {
                            store.entries.insert(e.name.clone(), e);
                        }
                    }
                }
                self.save_vault()?;
                self.vault_list(None)
            }
            Ok((Response::Error(e), _)) => Err(e),
            Ok(other) => Err(internal(format!("管理设备的回应无法理解：{other:?}"))),
            Err(e) => Err(err(ErrorCode::Unreachable, format!("连不上管理设备 {}：{e}", admin.name))),
        }
    }

    /// 设备被吊销时调用：把该设备曾接触过的所有现存条目记入待轮换清单并返回这些条目名。
    pub(crate) fn note_device_revoked_for_vault(&self, device_name: &str) -> Vec<String> {
        let mut rotated = Vec::new();
        {
            let mut store = self.vault.borrow_mut();
            let active_names: Vec<String> = store.entries.keys().cloned().collect();
            for name in active_names {
                let exposed = store.exposed_to.get(&name).is_some_and(|s| s.contains(device_name))
                    || store.entries.get(&name).is_some_and(|e| e.devices.iter().any(|d| d == device_name));
                if exposed {
                    rotated.push(name.clone());
                }
                if let Some(entry) = store.entries.get_mut(&name) {
                    entry.devices.retain(|d| d != device_name);
                }
            }
            if !rotated.is_empty() {
                let set = store.rotate_needed.entry(device_name.to_string()).or_default();
                for n in &rotated {
                    set.insert(n.clone());
                }
            }
        }
        let _ = self.save_vault();
        rotated.sort();
        rotated
    }

    /// 列出因设备被吊销而需要轮换的条目清单。
    pub(crate) fn vault_rotate_needed(&self, device: Option<&str>) -> Result<Vec<VaultRotateItem>, WireError> {
        let store = self.vault.borrow();
        let mut out = Vec::new();
        for (dev, entries) in &store.rotate_needed {
            if let Some(filter) = device {
                if dev != filter {
                    continue;
                }
            }
            if !entries.is_empty() {
                out.push(VaultRotateItem {
                    revoked_device: dev.clone(),
                    entries: entries.iter().cloned().collect(),
                });
            }
        }
        Ok(out)
    }

    /// 处理来自管理设备的 `Request::VaultPush`。
    pub(crate) fn handle_vault_push(&self, peer: NodeId, name: String, entry: Option<VaultCipherEntry>) -> Response {
        let Some(roster) = self.roster() else {
            return Response::Error(err(ErrorCode::Denied, "本机还没有加入任何网络"));
        };
        let is_admin = roster.device(peer).is_some_and(|d| d.role == DeviceRole::Admin);
        if !is_admin {
            return Response::Error(err(ErrorCode::Denied, "只有管理设备可以推送 Vault 条目"));
        }
        let me_name = self.me_name();
        {
            let mut store = self.vault.borrow_mut();
            match entry {
                Some(e) if e.devices.iter().any(|d| d == &me_name) => {
                    store.entries.insert(name, e);
                }
                _ => {
                    store.entries.remove(&name);
                }
            }
        }
        if let Err(e) = self.save_vault() {
            return Response::Error(e);
        }
        Response::Ok
    }

    /// 处理来自成员设备的 `Request::VaultPull`：只返回授权给该对端设备的密文条目，并逐条记审计。
    pub(crate) fn handle_vault_pull(&self, peer: NodeId) -> Response {
        let Some(roster) = self.roster() else {
            return Response::Error(err(ErrorCode::Denied, "本机还没有加入任何网络"));
        };
        let Some(peer_dev) = roster.device(peer) else {
            return Response::Error(err(ErrorCode::Denied, "设备不在名册里"));
        };
        let peer_name = peer_dev.name.clone();
        let entries: Vec<VaultCipherEntry> = {
            let store = self.vault.borrow();
            store
                .entries
                .values()
                .filter(|e| e.devices.iter().any(|d| d == &peer_name))
                .cloned()
                .collect()
        };
        for e in &entries {
            self.audit_vault(peer, PrincipalRef::Device(peer), &e.name, Outcome::Allow);
        }
        Response::VaultEntries(entries)
    }
}
