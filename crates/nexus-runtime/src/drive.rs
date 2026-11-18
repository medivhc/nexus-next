//! Drive（M4.1、M4.2）：工作区、内容存储、提交，以及和存储设备之间的同步。
//!
//! 每个工作区有一个「头」（最近的提交）和一份「工作树」（尚未提交的状态）。两者都是按内容寻址的
//! 目录树的根；写入只是产生新的对象和新的根（写时复制），旧的根不受影响。

use crate::{err, internal, load, save, CtlServer, Node};
use nexus_drive_core::chunk::Chunker;
use nexus_drive_core::path::{valid_workspace, DrivePath};
use nexus_drive_core::tree::{self, ChangeKind, DriveError, Entry, Store};
use nexus_drive_core::history::{self, Relation};
use nexus_drive_core::{ChunkRef, Commit, FileManifest, Hash};
use nexus_fabric::{read_msg, write_msg, Transport};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use nexus_sys::System;
use nexus_wire::{
    AgentCredential, CtlResponse, DriveChange, DriveCommitInfo, DriveEntryInfo, DriveRequest, DriveResponse, DriveSyncReport, DriveWhere,
    ErrorCode, Input, Request, Response, WireError, WorkspaceInfo,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io;

pub(crate) const KEY_DRIVE: &str = "drive-v2";

/// 一次取对象、查缺少的对象最多带这么多哈希（一帧之内）。
const BATCH: usize = 8192;
/// 问一台设备是不是存储设备的时限。
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// 提交号前缀至少这么长。
const MIN_PREFIX: usize = 8;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct DriveState {
    pub workspaces: BTreeMap<String, WsState>,
    /// 本机是否作为存储设备（保存登记过的工作区的完整副本）。
    pub serving: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct WsState {
    pub head: Option<Hash>,
    pub work: Hash,
    /// 保存完整副本的设备（名字）。存储设备自己登记的工作区这里为空。
    pub replicas: Vec<String>,
    /// 最近一次知道的存储设备上的头。
    pub remote: Option<Hash>,
}

impl DriveState {
    pub fn load<S: System>(sys: &S) -> io::Result<Self> {
        Ok(load(sys, KEY_DRIVE)?.unwrap_or_default())
    }
}

/// 接缝上的对象存储。写失败时对象缺失，之后读到时报告为 `Missing`。
pub(crate) struct Cas<'a, S>(pub &'a S);

impl<S: System> Store for Cas<'_, S> {
    fn get(&self, hash: &Hash) -> Option<Vec<u8>> {
        self.0.cas_get(&hash.0).ok().flatten()
    }
    fn put(&mut self, bytes: Vec<u8>) -> Hash {
        let h = Hash::of(&bytes);
        let _ = self.0.cas_put(&h.0, &bytes);
        h
    }
}

fn tree_err(e: DriveError, path: &DrivePath) -> WireError {
    match e {
        DriveError::NotFound => err(ErrorCode::NotFound, format!("没有 {path}")),
        DriveError::NotADirectory => err(ErrorCode::InvalidInput, format!("{path} 的上级不是目录")),
        DriveError::IsADirectory => err(ErrorCode::InvalidInput, format!("{path} 是目录")),
        DriveError::Missing(h) => internal(format!("本机缺少对象 {}", h.short())),
    }
}

fn parse(path: &str) -> Result<DrivePath, WireError> {
    path.parse::<DrivePath>().map_err(|m| err(ErrorCode::InvalidInput, m))
}

fn kind_name(k: ChangeKind) -> &'static str {
    match k {
        ChangeKind::Added => "added",
        ChangeKind::Modified => "modified",
        ChangeKind::Deleted => "deleted",
    }
}

fn entry_info(name: String, e: &Entry) -> DriveEntryInfo {
    match e {
        Entry::File { size, exec, .. } => DriveEntryInfo { name, kind: "file".into(), size: *size, exec: *exec, target: None },
        Entry::Dir { .. } => DriveEntryInfo { name, kind: "dir".into(), size: 0, exec: false, target: None },
        Entry::Symlink { target } => {
            DriveEntryInfo { name, kind: "symlink".into(), size: target.len() as u64, exec: false, target: Some(target.clone()) }
        }
    }
}

impl<T: Transport, S: System> Node<T, S> {
    pub(crate) async fn drive(&self, req: DriveRequest, agent: Option<AgentCredential>, mut server: CtlServer) {
        if let Err(e) = self.drive_allowed(&req, agent.as_ref()) {
            let _ = server.tx.send(CtlResponse::Error(e)).await;
            return;
        }
        let result = match req {
            DriveRequest::Cat { path, at, offset, length } => return self.drive_cat(&path, at.as_deref(), offset, length, server).await,
            DriveRequest::Put { path, exec } => self.drive_put(&path, exec, &mut server).await,
            DriveRequest::Create { ws, replicas } => self.drive_create(&ws, replicas).await,
            DriveRequest::List => self.drive_list(agent.as_ref()),
            DriveRequest::Serve { on } => self.drive_serve(on),
            DriveRequest::Sync { ws } => self.drive_sync(&ws).await.map(DriveResponse::Synced),
            DriveRequest::Merge { ws } => self.drive_merge(&ws).await,
            DriveRequest::Where { path } => self.drive_where(&path),
            DriveRequest::Ls { path, at } => self.drive_ls(&path, at.as_deref()),
            DriveRequest::Symlink { path, target } => {
                self.drive_set(&path, Entry::Symlink { target }).map(|_| DriveResponse::Written { path, size: 0 })
            }
            DriveRequest::Rm { path, recursive } => self.drive_rm(&path, recursive),
            DriveRequest::Status { ws } => self.drive_status(&ws),
            DriveRequest::Commit { ws, message } => self.drive_commit(&ws, message).await,
            DriveRequest::Log { ws } => self.drive_log(&ws),
        };
        let resp = match result {
            Ok(r) => CtlResponse::Drive(r),
            Err(e) => CtlResponse::Error(e),
        };
        let _ = server.tx.send(resp).await;
    }

    fn ws(&self, name: &str) -> Result<WsState, WireError> {
        self.drive_state
            .borrow()
            .workspaces
            .get(name)
            .cloned()
            .ok_or_else(|| err(ErrorCode::NotFound, format!("没有工作区 {name}")))
    }

    fn update_ws(&self, name: &str, f: impl FnOnce(&mut WsState)) -> Result<(), WireError> {
        let mut state = self.drive_state.borrow_mut();
        let ws = state.workspaces.get_mut(name).ok_or_else(|| err(ErrorCode::NotFound, format!("没有工作区 {name}")))?;
        f(ws);
        save(&self.system, KEY_DRIVE, &*state).map_err(internal)
    }

    async fn drive_create(&self, name: &str, replicas: Vec<String>) -> Result<DriveResponse, WireError> {
        if !valid_workspace(name) {
            return Err(err(ErrorCode::InvalidInput, format!("工作区名只能是小写字母、数字和连字符，1 到 32 个字符：{name}")));
        }
        if self.drive_state.borrow().workspaces.contains_key(name) {
            return Err(err(ErrorCode::InvalidInput, format!("已经有工作区 {name}")));
        }
        // 副本：指定的设备，或者网络里开了 serve 的设备。
        let replicas = if replicas.is_empty() {
            self.storage_devices().await
        } else {
            for r in &replicas {
                if self.resolve(r)?.node_id == self.me() {
                    return Err(err(ErrorCode::InvalidInput, format!("{r} 就是本机")));
                }
            }
            replicas
        };
        for r in &replicas {
            let device = self.resolve(r)?;
            match self.request(&device, Request::DriveRegister { ws: name.to_string() }, None).await {
                Ok((Response::Ok, _)) => {}
                Ok((Response::Error(e), _)) => return Err(e),
                Ok(_) => return Err(internal("存储设备的回应无法理解")),
                Err(e) => return Err(err(ErrorCode::Unreachable, format!("连不上存储设备 {r}：{e}"))),
            }
        }
        self.insert_ws(name, replicas)?;
        Ok(DriveResponse::Created { ws: name.to_string() })
    }

    fn insert_ws(&self, name: &str, replicas: Vec<String>) -> Result<(), WireError> {
        let mut state = self.drive_state.borrow_mut();
        if !state.workspaces.contains_key(name) {
            let work = tree::empty(&mut Cas(&self.system));
            state.workspaces.insert(name.to_string(), WsState { head: None, work, replicas, remote: None });
        }
        save(&self.system, KEY_DRIVE, &*state).map_err(internal)
    }

    /// 网络里开了 serve 的其他设备（名字）。连不上的设备不算。
    async fn storage_devices(&self) -> Vec<String> {
        let Some(roster) = self.roster() else { return Vec::new() };
        let others: Vec<_> = roster.devices.into_iter().filter(|d| d.node_id != self.me()).collect();
        let answers = futures_util::future::join_all(others.iter().map(|d| async move {
            match tokio::time::timeout(PROBE_TIMEOUT, self.request(d, Request::DriveInfo, None)).await {
                Ok(Ok((Response::DriveInfo { serving: true, .. }, _))) => Some(d.name.clone()),
                _ => None,
            }
        }))
        .await;
        answers.into_iter().flatten().collect()
    }

    fn drive_serve(&self, on: Option<bool>) -> Result<DriveResponse, WireError> {
        let mut state = self.drive_state.borrow_mut();
        if let Some(on) = on {
            state.serving = on;
            save(&self.system, KEY_DRIVE, &*state).map_err(internal)?;
        }
        Ok(DriveResponse::Serving { on: state.serving })
    }

    fn head_tree(&self, ws: &WsState) -> Result<Option<Hash>, WireError> {
        ws.head.map(|c| self.commit(c).map(|c| c.tree)).transpose()
    }

    fn commit(&self, hash: Hash) -> Result<Commit, WireError> {
        Cas(&self.system)
            .get(&hash)
            .and_then(|b| Commit::decode(&b))
            .ok_or_else(|| internal(format!("本机缺少提交 {}", hash.short())))
    }

    fn changes(&self, ws: &WsState) -> Result<Vec<DriveChange>, WireError> {
        let base = self.head_tree(ws)?;
        let d = tree::diff(&Cas(&self.system), base, ws.work).map_err(|e| internal(format!("{e:?}")))?;
        Ok(d.into_iter().map(|c| DriveChange { kind: kind_name(c.kind).into(), path: c.path }).collect())
    }

    fn drive_list(&self, agent: Option<&AgentCredential>) -> Result<DriveResponse, WireError> {
        let all: Vec<(String, WsState)> = self.drive_state.borrow().workspaces.clone().into_iter().collect();
        let mut out = Vec::new();
        for (name, ws) in all {
            if agent.is_some() && self.drive_agent_may(agent, &name, false).is_err() {
                continue;
            }
            let changes = self.changes(&ws)?.len() as u32;
            out.push(WorkspaceInfo { name, head: ws.head.map(|h| h.to_string()), changes });
        }
        Ok(DriveResponse::Workspaces(out))
    }

    /// 在提交历史里按前缀找提交（从头沿父提交往回走）。
    fn find_commit(&self, ws: &WsState, prefix: &str) -> Result<Commit, WireError> {
        let prefix = prefix.to_ascii_lowercase();
        if prefix.len() < MIN_PREFIX || !prefix.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(err(ErrorCode::InvalidInput, format!("提交号至少写 {MIN_PREFIX} 个十六进制字符：{prefix}")));
        }
        let Some(head) = ws.head else { return Err(err(ErrorCode::NotFound, format!("没有提交 {prefix}"))) };
        let all = history::ancestors(&|h: &Hash| self.commit(*h).ok(), head)
            .map_err(|h| internal(format!("本机缺少提交 {}", h.short())))?;
        match all.into_iter().find(|h| h.to_string().starts_with(&prefix)) {
            Some(h) => self.commit(h),
            None => Err(err(ErrorCode::NotFound, format!("没有提交 {prefix}"))),
        }
    }

    /// 要看的根：`at` 为空时是工作树，否则是那次提交的树。
    fn root(&self, ws: &WsState, at: Option<&str>) -> Result<Hash, WireError> {
        match at {
            None => Ok(ws.work),
            Some(p) => Ok(self.find_commit(ws, p)?.tree),
        }
    }

    fn drive_ls(&self, path: &str, at: Option<&str>) -> Result<DriveResponse, WireError> {
        let p = parse(path)?;
        let ws = self.ws(&p.ws)?;
        let root = self.root(&ws, at)?;
        let cas = Cas(&self.system);
        let entry = tree::lookup(&cas, root, &p.parts).map_err(|e| tree_err(e, &p))?;
        let entries = match entry {
            None => return Err(err(ErrorCode::NotFound, format!("没有 {p}"))),
            Some(Entry::Dir { tree }) => {
                let t = tree::load(&cas, tree).map_err(|e| tree_err(e, &p))?;
                t.entries.iter().map(|(n, e)| entry_info(n.clone(), e)).collect()
            }
            Some(e) => vec![entry_info(p.parts.last().cloned().unwrap_or_default(), &e)],
        };
        Ok(DriveResponse::Entries { path: p.to_string(), entries })
    }

    async fn drive_cat(&self, path: &str, at: Option<&str>, offset: u64, length: Option<u64>, server: CtlServer) {
        let prepared = (|| {
            let p = parse(path)?;
            let ws = self.ws(&p.ws)?;
            let root = self.root(&ws, at)?;
            let cas = Cas(&self.system);
            match tree::lookup(&cas, root, &p.parts).map_err(|e| tree_err(e, &p))? {
                None => Err(err(ErrorCode::NotFound, format!("没有 {p}"))),
                Some(Entry::Dir { .. }) => Err(err(ErrorCode::InvalidInput, format!("{p} 是目录"))),
                Some(Entry::Symlink { target }) => Err(err(ErrorCode::InvalidInput, format!("{p} 是指向 {target} 的符号链接"))),
                Some(Entry::File { manifest, .. }) => cas
                    .get(&manifest)
                    .and_then(|b| FileManifest::decode(&b))
                    .ok_or_else(|| internal(format!("本机缺少 {p} 的清单"))),
            }
        })();
        let manifest = match prepared {
            Ok(m) => m,
            Err(e) => {
                let _ = server.tx.send(CtlResponse::Error(e)).await;
                return;
            }
        };
        // 本机没有的块从存储设备取（只取这个范围用到的）。
        let spans = manifest.spans(offset, length.unwrap_or(u64::MAX));
        let missing: Vec<Hash> = spans.iter().map(|s| s.hash).filter(|h| !self.system.cas_has(&h.0).unwrap_or(false)).collect();
        if !missing.is_empty() {
            let ws = path.split_once(':').map(|(w, _)| w.to_string()).unwrap_or_default();
            if let Err(e) = self.fetch(&ws, &missing).await {
                let _ = server.tx.send(CtlResponse::Error(e)).await;
                return;
            }
        }
        if server.tx.send(CtlResponse::Ok).await.is_err() {
            return;
        }
        for span in spans {
            let Some(chunk) = self.system.cas_get(&span.hash.0).ok().flatten() else {
                let _ = server.tx.send(CtlResponse::Error(internal(format!("本机缺少块 {}", span.hash.short())))).await;
                return;
            };
            let part = chunk.get(span.from as usize..span.to as usize).unwrap_or_default().to_vec();
            if server.tx.send(CtlResponse::PipeData(part)).await.is_err() {
                return;
            }
        }
        let _ = server.tx.send(CtlResponse::PipeEnd).await;
    }

    async fn drive_put(&self, path: &str, exec: bool, server: &mut CtlServer) -> Result<DriveResponse, WireError> {
        let p = parse(path)?;
        if p.parts.is_empty() {
            return Err(err(ErrorCode::InvalidInput, "要写的是文件，不是工作区的根"));
        }
        self.ws(&p.ws)?;
        let mut cas = Cas(&self.system);
        let mut chunker = Chunker::default();
        // 块边读边存，内存里只留清单。
        let mut chunks: Vec<ChunkRef> = Vec::new();
        let store = |new: Vec<Vec<u8>>, chunks: &mut Vec<ChunkRef>, cas: &mut Cas<S>| {
            for c in new {
                let len = c.len() as u32;
                chunks.push(ChunkRef { hash: cas.put(c), len });
            }
        };
        loop {
            match server.rx.recv().await {
                Some(Input::Stdin(b)) => store(chunker.push(&b), &mut chunks, &mut cas),
                Some(Input::CloseStdin) => break,
                Some(Input::Cancel) | None => return Err(err(ErrorCode::InvalidInput, "写入被取消")),
                Some(_) => {}
            }
        }
        store(chunker.finish(), &mut chunks, &mut cas);
        let manifest = FileManifest { size: chunks.iter().map(|c| c.len as u64).sum(), chunks };
        let size = manifest.size;
        let mh = cas.put(manifest.encode());
        self.drive_set(path, Entry::File { manifest: mh, size, exec })?;
        Ok(DriveResponse::Written { path: p.to_string(), size })
    }

    /// 把工作树里的一项设成 `entry`。
    fn drive_set(&self, path: &str, entry: Entry) -> Result<Hash, WireError> {
        let p = parse(path)?;
        if p.parts.is_empty() {
            return Err(err(ErrorCode::InvalidInput, "不能替换工作区的根"));
        }
        let ws = self.ws(&p.ws)?;
        let work = tree::set(&mut Cas(&self.system), ws.work, &p.parts, entry).map_err(|e| tree_err(e, &p))?;
        self.update_ws(&p.ws, |w| w.work = work)?;
        Ok(work)
    }

    fn drive_rm(&self, path: &str, recursive: bool) -> Result<DriveResponse, WireError> {
        let p = parse(path)?;
        let ws = self.ws(&p.ws)?;
        let work = tree::remove(&mut Cas(&self.system), ws.work, &p.parts, recursive).map_err(|e| match e {
            DriveError::IsADirectory if !p.parts.is_empty() => err(ErrorCode::InvalidInput, format!("{p} 是目录：删除目录要加 -r")),
            DriveError::IsADirectory => err(ErrorCode::InvalidInput, "不能删除工作区的根"),
            e => tree_err(e, &p),
        })?;
        self.update_ws(&p.ws, |w| w.work = work)?;
        Ok(DriveResponse::Removed { path: p.to_string() })
    }

    fn drive_status(&self, name: &str) -> Result<DriveResponse, WireError> {
        let ws = self.ws(name)?;
        Ok(DriveResponse::Changes(self.changes(&ws)?))
    }

    async fn drive_commit(&self, name: &str, message: String) -> Result<DriveResponse, WireError> {
        let ws = self.ws(name)?;
        if self.changes(&ws)?.is_empty() {
            return Err(err(ErrorCode::InvalidInput, format!("工作区 {name} 没有可提交的改动")));
        }
        let commit = Commit {
            tree: ws.work,
            parents: ws.head.into_iter().collect(),
            device: self.my_name().unwrap_or_else(|_| self.configured_name.clone()),
            time_ms: self.system.now().0,
            message,
        };
        let h = Cas(&self.system).put(commit.encode());
        self.update_ws(name, |w| w.head = Some(h))?;
        let sync = self.push(name).await;
        Ok(DriveResponse::Committed { commit: h.to_string(), sync })
    }

    fn drive_log(&self, name: &str) -> Result<DriveResponse, WireError> {
        let ws = self.ws(name)?;
        let mut out = Vec::new();
        let mut next = ws.head;
        while let Some(h) = next {
            let c = self.commit(h)?;
            next = c.parents.first().copied();
            out.push(DriveCommitInfo { commit: h.to_string(), message: c.message, device: c.device, time_ms: c.time_ms });
        }
        Ok(DriveResponse::Log(out))
    }
}

// ---------------------------------------------------------------- M4.2：同步、授权、可解释

impl<T: Transport, S: System> Node<T, S> {
    /// agent 的本机请求：读要 drive.read，写要 drive.write（资源 `<本机>/drive/<工作区>`）；建工作区和 serve 是管理操作。
    fn drive_allowed(&self, req: &DriveRequest, agent: Option<&AgentCredential>) -> Result<(), WireError> {
        if agent.is_none() {
            return Ok(());
        }
        let ws_of = |p: &str| p.split_once(':').map_or(p.to_string(), |(w, _)| w.to_string());
        match req {
            DriveRequest::Create { .. } | DriveRequest::Serve { .. } => Err(err(ErrorCode::Denied, "agent 不能执行管理操作")),
            DriveRequest::List => Ok(()),
            DriveRequest::Ls { path, .. } | DriveRequest::Cat { path, .. } | DriveRequest::Where { path } => {
                self.drive_agent_may(agent, &ws_of(path), false)
            }
            DriveRequest::Status { ws } | DriveRequest::Log { ws } => self.drive_agent_may(agent, ws, false),
            DriveRequest::Put { path, .. } | DriveRequest::Symlink { path, .. } | DriveRequest::Rm { path, .. } => {
                self.drive_agent_may(agent, &ws_of(path), true)
            }
            DriveRequest::Commit { ws, .. } | DriveRequest::Merge { ws } | DriveRequest::Sync { ws } => self.drive_agent_may(agent, ws, true),
        }
    }

    fn drive_agent_may(&self, agent: Option<&AgentCredential>, ws: &str, write: bool) -> Result<(), WireError> {
        let body = if write {
            Request::DrivePush { ws: ws.to_string(), head: [0; 32], count: 0 }
        } else {
            Request::DriveGet { ws: ws.to_string(), hashes: Vec::new() }
        };
        self.authorize_local(agent, &body)
    }

    /// 从 `head` 可以到达的对象（提交、树、清单，`chunks` 时还有块），以及其中本机没有的。
    fn closure(&self, head: Hash, chunks: bool) -> (Vec<Hash>, Vec<Hash>) {
        let cas = Cas(&self.system);
        let mut seen = std::collections::BTreeSet::new();
        let mut missing = Vec::new();
        let mut queue = vec![(head, 'c')];
        while let Some((h, kind)) = queue.pop() {
            if !seen.insert(h) {
                continue;
            }
            if kind == 'k' {
                if !self.system.cas_has(&h.0).unwrap_or(false) {
                    missing.push(h);
                }
                continue;
            }
            let Some(bytes) = cas.get(&h) else {
                missing.push(h);
                continue;
            };
            match kind {
                'c' => {
                    if let Some(c) = Commit::decode(&bytes) {
                        queue.extend(c.parents.iter().map(|p| (*p, 'c')));
                        queue.push((c.tree, 't'));
                    }
                }
                't' => {
                    if let Some(t) = tree::Tree::decode(&bytes) {
                        for e in t.entries.values() {
                            match e {
                                Entry::Dir { tree } => queue.push((*tree, 't')),
                                Entry::File { manifest, .. } => queue.push((*manifest, 'm')),
                                Entry::Symlink { .. } => {}
                            }
                        }
                    }
                }
                _ => {
                    if let (true, Some(m)) = (chunks, FileManifest::decode(&bytes)) {
                        queue.extend(m.chunks.iter().map(|c| (c.hash, 'k')));
                    }
                }
            }
        }
        (seen.into_iter().collect(), missing)
    }

    /// 从工作区的存储设备取对象，存到本机。按副本的顺序，第一个能给的就用它。
    async fn fetch(&self, ws: &str, hashes: &[Hash]) -> Result<(), WireError> {
        let replicas = self.ws(ws)?.replicas;
        if replicas.is_empty() {
            return Err(internal(format!("本机缺少 {} 个对象，工作区 {ws} 没有存储设备", hashes.len())));
        }
        let mut last = String::new();
        for r in &replicas {
            match self.fetch_from(r, ws, hashes).await {
                Ok(()) => return Ok(()),
                Err(e) => last = format!("{r}：{}", e.message),
            }
        }
        Err(err(ErrorCode::Unreachable, format!("内容不在本机，存储设备也取不到（{last}）")))
    }

    async fn fetch_from(&self, replica: &str, ws: &str, hashes: &[Hash]) -> Result<(), WireError> {
        let device = self.resolve(replica)?;
        for batch in hashes.chunks(BATCH) {
            let body = Request::DriveGet { ws: ws.to_string(), hashes: batch.iter().map(|h| h.0).collect() };
            let mut stream = self.open_stream(&device, body, None).await.map_err(|e| match e {
                crate::OpenError::Remote(e) => e,
                crate::OpenError::StaleRoster => err(ErrorCode::Unreachable, "名册不一致"),
                crate::OpenError::Io(e) => err(ErrorCode::Unreachable, e.to_string()),
            })?;
            for h in batch {
                let obj: Option<Vec<u8>> = read_msg(&mut stream).await.map_err(|e| err(ErrorCode::Unreachable, e.to_string()))?;
                let bytes = obj.ok_or_else(|| err(ErrorCode::NotFound, format!("{replica} 也没有对象 {}", h.short())))?;
                if Hash::of(&bytes) != *h {
                    return Err(err(ErrorCode::Internal, format!("{replica} 给的对象 {} 内容不对", h.short())));
                }
                self.system.cas_put(&h.0, &bytes).map_err(internal)?;
            }
        }
        Ok(())
    }

    /// 取齐从 `head` 可以到达的目录结构（不含块）。
    async fn fetch_structure(&self, ws: &str, head: Hash) -> Result<(), WireError> {
        loop {
            let (_, missing) = self.closure(head, false);
            if missing.is_empty() {
                return Ok(());
            }
            self.fetch(ws, &missing).await?;
        }
    }

    /// 把本机的头推给每个存储设备。
    async fn push(&self, name: &str) -> DriveSyncReport {
        let Ok(ws) = self.ws(name) else { return report("local", None, None, "没有这个工作区") };
        let head = ws.head;
        if ws.replicas.is_empty() {
            return report("local", head, None, "没有存储设备：只在本机");
        }
        let Some(h) = head else { return report("same", None, ws.remote, "") };
        let mut out = report("pushed", head, Some(h), "");
        for r in &ws.replicas {
            match self.push_to(r, name, h).await {
                Ok(Response::DrivePushed { accepted: true, .. }) => {
                    let _ = self.update_ws(name, |w| w.remote = Some(h));
                }
                Ok(Response::DrivePushed { accepted: false, head: theirs }) => {
                    let theirs = theirs.map(Hash);
                    let _ = self.update_ws(name, |w| w.remote = theirs);
                    return report("diverged", head, theirs, &format!("{r} 上有本机没有的提交：先 nexus drive sync {name}，再 merge"));
                }
                Ok(other) => return report("offline", head, ws.remote, &format!("{r} 的回应无法理解：{other:?}")),
                Err(e) => out = report("offline", head, ws.remote, &format!("连不上 {r}（{}）：提交只在本机，之后 nexus drive sync {name}", e.message)),
            }
        }
        out
    }

    async fn push_to(&self, replica: &str, ws: &str, head: Hash) -> Result<Response, WireError> {
        let device = self.resolve(replica)?;
        let (all, _) = self.closure(head, true);
        let mut missing = Vec::new();
        for batch in all.chunks(BATCH) {
            let body = Request::DriveHave { ws: ws.to_string(), hashes: batch.iter().map(|h| h.0).collect() };
            match self.request(&device, body, None).await.map_err(|e| err(ErrorCode::Unreachable, e.to_string()))? {
                (Response::DriveMissing(m), _) => missing.extend(m),
                (Response::Error(e), _) => return Err(e),
                _ => return Err(internal("存储设备的回应无法理解")),
            }
        }
        let body = Request::DrivePush { ws: ws.to_string(), head: head.0, count: missing.len() as u32 };
        let mut stream = self.open_stream(&device, body, None).await.map_err(|e| match e {
            crate::OpenError::Remote(e) => e,
            crate::OpenError::StaleRoster => err(ErrorCode::Unreachable, "名册不一致"),
            crate::OpenError::Io(e) => err(ErrorCode::Unreachable, e.to_string()),
        })?;
        for h in &missing {
            let bytes = self.system.cas_get(h).ok().flatten().ok_or_else(|| internal("本机缺少要推送的对象"))?;
            write_msg(&mut stream, &bytes).await.map_err(|e| err(ErrorCode::Unreachable, e.to_string()))?;
        }
        read_msg::<_, Response>(&mut stream).await.map_err(|e| err(ErrorCode::Unreachable, e.to_string()))
    }

    /// 存储设备上这个工作区的头；没有存储设备或都连不上时为空。
    async fn remote_head(&self, ws: &str, replicas: &[String]) -> Result<Option<Option<Hash>>, WireError> {
        for r in replicas {
            let Ok(device) = self.resolve(r) else { continue };
            if let Ok(Ok((Response::DriveInfo { workspaces, .. }, _))) =
                tokio::time::timeout(PROBE_TIMEOUT, self.request(&device, Request::DriveInfo, None)).await
            {
                if let Some((_, head)) = workspaces.into_iter().find(|(n, _)| n == ws) {
                    return Ok(Some(head.map(Hash)));
                }
            }
        }
        Ok(None)
    }

    async fn drive_sync(&self, name: &str) -> Result<DriveSyncReport, WireError> {
        if !valid_workspace(name) {
            return Err(err(ErrorCode::InvalidInput, format!("工作区名不合法：{name}")));
        }
        // 本机还没有这个工作区：问存储设备。
        if self.ws(name).is_err() {
            let mut found = Vec::new();
            for r in self.storage_devices().await {
                if let Ok(Some(_)) = self.remote_head(name, std::slice::from_ref(&r)).await {
                    found.push(r);
                }
            }
            if found.is_empty() {
                return Err(err(ErrorCode::NotFound, format!("本机和存储设备上都没有工作区 {name}")));
            }
            self.insert_ws(name, found)?;
        }
        let ws = self.ws(name)?;
        if ws.replicas.is_empty() {
            return Ok(report("local", ws.head, None, "没有存储设备：只在本机"));
        }
        let Some(remote) = self.remote_head(name, &ws.replicas).await? else {
            return Ok(report("offline", ws.head, ws.remote, "存储设备都连不上"));
        };
        let Some(remote) = remote else {
            // 存储设备上还没有提交。
            return Ok(if ws.head.is_some() { self.push(name).await } else { report("same", None, None, "") });
        };
        self.fetch_structure(name, remote).await?;
        self.update_ws(name, |w| w.remote = Some(remote))?;
        let Some(local) = ws.head else {
            self.fast_forward(name, remote)?;
            return Ok(report("pulled", Some(remote), Some(remote), ""));
        };
        let get = |h: &Hash| self.commit(*h).ok();
        match history::relation(&get, local, remote).map_err(|h| internal(format!("本机缺少提交 {}", h.short())))? {
            Relation::Same => Ok(report("same", Some(local), Some(remote), "")),
            Relation::Ahead => Ok(self.push(name).await),
            Relation::Behind => {
                self.fast_forward(name, remote)?;
                Ok(report("pulled", Some(remote), Some(remote), ""))
            }
            Relation::Diverged { .. } => Ok(report(
                "diverged",
                Some(local),
                Some(remote),
                &format!("本机和存储设备各有对方没有的提交：nexus drive merge {name}"),
            )),
        }
    }

    /// 头快进到 `to`；本机有未提交的改动时把它们带到新的头上（冲突时拒绝）。
    fn fast_forward(&self, name: &str, to: Hash) -> Result<(), WireError> {
        let ws = self.ws(name)?;
        let new_tree = self.commit(to)?.tree;
        let old_tree = self.head_tree(&ws)?;
        let work = if Some(ws.work) == old_tree || (old_tree.is_none() && self.changes(&ws)?.is_empty()) {
            new_tree
        } else {
            let (merged, conflicts) = tree::merge(&mut Cas(&self.system), old_tree, ws.work, new_tree, "remote")
                .map_err(|e| internal(format!("{e:?}")))?;
            if !conflicts.is_empty() {
                return Err(err(
                    ErrorCode::InvalidState,
                    format!("本机未提交的改动和同步来的修改冲突（{}）：先提交本机的改动再同步", conflicts.join("、")),
                ));
            }
            merged
        };
        self.update_ws(name, |w| {
            w.head = Some(to);
            w.work = work;
        })
    }

    async fn drive_merge(&self, name: &str) -> Result<DriveResponse, WireError> {
        let ws = self.ws(name)?;
        let (Some(local), Some(remote)) = (ws.head, ws.remote) else {
            return Err(err(ErrorCode::InvalidState, format!("没有要合并的：先 nexus drive sync {name}")));
        };
        if !self.changes(&ws)?.is_empty() {
            return Err(err(ErrorCode::InvalidState, "本机有未提交的改动：先提交再合并"));
        }
        let get = |h: &Hash| self.commit(*h).ok();
        let base = match history::relation(&get, local, remote).map_err(|h| internal(format!("本机缺少提交 {}", h.short())))? {
            Relation::Diverged { base } => base,
            _ => return Err(err(ErrorCode::InvalidState, format!("没有分叉，不需要合并：nexus drive sync {name}"))),
        };
        let theirs = self.commit(remote)?;
        let base_tree = base.map(|b| self.commit(b).map(|c| c.tree)).transpose()?;
        let ours_tree = self.commit(local)?.tree;
        let (merged, conflicts) = tree::merge(&mut Cas(&self.system), base_tree, ours_tree, theirs.tree, &theirs.device)
            .map_err(|e| match e {
                DriveError::Missing(_) => err(ErrorCode::Unreachable, "合并需要的目录结构不在本机：先同步"),
                e => internal(format!("{e:?}")),
            })?;
        let commit = Commit {
            tree: merged,
            parents: vec![local, remote],
            device: self.my_name().unwrap_or_else(|_| self.configured_name.clone()),
            time_ms: self.system.now().0,
            message: format!("合并 {} 的修改", theirs.device),
        };
        let h = Cas(&self.system).put(commit.encode());
        self.update_ws(name, |w| {
            w.head = Some(h);
            w.work = merged;
        })?;
        let sync = self.push(name).await;
        Ok(DriveResponse::Merged { commit: h.to_string(), conflicts, sync })
    }

    fn drive_where(&self, path: &str) -> Result<DriveResponse, WireError> {
        let p = parse(path)?;
        let ws = self.ws(&p.ws)?;
        let cas = Cas(&self.system);
        let entry = tree::lookup(&cas, ws.work, &p.parts).map_err(|e| tree_err(e, &p))?.ok_or_else(|| err(ErrorCode::NotFound, format!("没有 {p}")))?;
        let manifests: Vec<Hash> = match entry {
            Entry::File { manifest, .. } => vec![manifest],
            Entry::Symlink { .. } => vec![],
            Entry::Dir { tree } => tree::files(&cas, tree)
                .map_err(|e| tree_err(e, &p))?
                .into_values()
                .filter_map(|e| match e {
                    Entry::File { manifest, .. } => Some(manifest),
                    _ => None,
                })
                .collect(),
        };
        let (mut total, mut local) = (0u64, 0u64);
        for m in manifests {
            let Some(m) = cas.get(&m).and_then(|b| FileManifest::decode(&b)) else { continue };
            for c in m.chunks {
                total += c.len as u64;
                if self.system.cas_has(&c.hash.0).unwrap_or(false) {
                    local += c.len as u64;
                }
            }
        }
        let uncommitted = self.changes(&ws)?.len() as u32;
        let synced = !ws.replicas.is_empty() && ws.head.is_some() && ws.remote == ws.head;
        Ok(DriveResponse::Where(DriveWhere { path: p.to_string(), replicas: ws.replicas, synced, uncommitted, total_bytes: total, local_bytes: local }))
    }

    // ------------------------------------------------------------ 存储设备一侧

    /// 对方问：是不是存储设备、有哪些工作区（只列出对方能读的）。
    pub(crate) fn drive_info(&self, may_read: impl Fn(&str) -> bool) -> Response {
        let state = self.drive_state.borrow();
        let workspaces = state
            .workspaces
            .iter()
            .filter(|(n, _)| may_read(n))
            .map(|(n, w)| (n.clone(), w.head.map(|h| h.0)))
            .collect();
        Response::DriveInfo { serving: state.serving, workspaces }
    }

    pub(crate) fn drive_register(&self, ws: &str) -> Response {
        if !self.drive_state.borrow().serving {
            return Response::Error(err(ErrorCode::InvalidState, "这台设备没有开 serve（nexus drive serve on）"));
        }
        match self.insert_ws(ws, Vec::new()) {
            Ok(()) => Response::Ok,
            Err(e) => Response::Error(e),
        }
    }

    pub(crate) fn drive_have(&self, ws: &str, hashes: Vec<[u8; 32]>) -> Response {
        if self.ws(ws).is_err() {
            return Response::Error(err(ErrorCode::NotFound, format!("没有工作区 {ws}")));
        }
        Response::DriveMissing(hashes.into_iter().filter(|h| !self.system.cas_has(h).unwrap_or(false)).collect())
    }

    /// 给对象。哈希就是内容的身份：拿得到哈希，说明对方读到过引用它的目录结构。
    pub(crate) async fn serve_drive_get<St: AsyncRead + AsyncWrite + Unpin>(&self, mut stream: St, hashes: Vec<[u8; 32]>) {
        if write_msg(&mut stream, &Response::Ok).await.is_err() {
            return;
        }
        for h in hashes {
            let obj = self.system.cas_get(&h).ok().flatten();
            if write_msg(&mut stream, &obj).await.is_err() {
                return;
            }
        }
        let _ = stream.shutdown().await;
    }

    /// 收推送：收下对象，检查新头的历史完整、是现在的头的后代，然后更新。
    pub(crate) async fn serve_drive_push<St: AsyncRead + AsyncWrite + Unpin>(&self, mut stream: St, ws: String, head: [u8; 32], count: u32) {
        if write_msg(&mut stream, &Response::Ok).await.is_err() {
            return;
        }
        for _ in 0..count {
            let Ok(bytes) = read_msg::<_, Vec<u8>>(&mut stream).await else { return };
            let h = Hash::of(&bytes);
            let _ = self.system.cas_put(&h.0, &bytes);
        }
        let head = Hash(head);
        let resp = (|| {
            let (_, missing) = self.closure(head, true);
            if !missing.is_empty() {
                return Response::Error(err(ErrorCode::InvalidInput, format!("推送不完整：缺少 {} 个对象", missing.len())));
            }
            let w = match self.ws(&ws) {
                Ok(w) => w,
                Err(e) => return Response::Error(e),
            };
            let get = |h: &Hash| self.commit(*h).ok();
            let accept = match w.head {
                None => true,
                Some(cur) => matches!(history::relation(&get, head, cur), Ok(Relation::Ahead | Relation::Same)),
            };
            if !accept {
                return Response::DrivePushed { accepted: false, head: w.head.map(|h| h.0) };
            }
            // 存储设备上的工作树跟着头走，除非这台设备自己有未提交的改动。
            let follow = w.head.is_none() || self.head_tree(&w).ok().flatten() == Some(w.work);
            let tree = self.commit(head).map(|c| c.tree);
            let result = self.update_ws(&ws, |s| {
                s.head = Some(head);
                if let (true, Ok(t)) = (follow, tree) {
                    s.work = t;
                }
            });
            match result {
                Ok(()) => Response::DrivePushed { accepted: true, head: Some(head.0) },
                Err(e) => Response::Error(e),
            }
        })();
        let _ = write_msg(&mut stream, &resp).await;
        let _ = stream.shutdown().await;
    }
}

fn report(state: &str, head: Option<Hash>, remote: Option<Hash>, message: &str) -> DriveSyncReport {
    DriveSyncReport {
        state: state.into(),
        head: head.map(|h| h.to_string()),
        remote: remote.map(|h| h.to_string()),
        message: message.into(),
    }
}
