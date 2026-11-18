//! Drive（M4.1）：本机的工作区、内容存储和提交。
//!
//! 每个工作区有一个「头」（最近的提交）和一份「工作树」（尚未提交的状态）。两者都是按内容寻址的
//! 目录树的根；写入只是产生新的对象和新的根（写时复制），旧的根不受影响。

use crate::{err, internal, load, save, CtlServer, Node};
use nexus_drive_core::chunk::Chunker;
use nexus_drive_core::path::{valid_workspace, DrivePath};
use nexus_drive_core::tree::{self, ChangeKind, DriveError, Entry, Store};
use nexus_drive_core::{ChunkRef, Commit, FileManifest, Hash};
use nexus_fabric::Transport;
use nexus_sys::System;
use nexus_wire::{
    CtlResponse, DriveChange, DriveCommitInfo, DriveEntryInfo, DriveRequest, DriveResponse, ErrorCode, Input, WireError, WorkspaceInfo,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io;

pub(crate) const KEY_DRIVE: &str = "drive";

/// 提交号前缀至少这么长。
const MIN_PREFIX: usize = 8;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct DriveState {
    pub workspaces: BTreeMap<String, WsState>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct WsState {
    pub head: Option<Hash>,
    pub work: Hash,
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
    pub(crate) async fn drive(&self, req: DriveRequest, mut server: CtlServer) {
        let result = match req {
            DriveRequest::Cat { path, at, offset, length } => return self.drive_cat(&path, at.as_deref(), offset, length, server).await,
            DriveRequest::Put { path, exec } => self.drive_put(&path, exec, &mut server).await,
            DriveRequest::Create { ws } => self.drive_create(&ws),
            DriveRequest::List => self.drive_list(),
            DriveRequest::Ls { path, at } => self.drive_ls(&path, at.as_deref()),
            DriveRequest::Symlink { path, target } => {
                self.drive_set(&path, Entry::Symlink { target }).map(|_| DriveResponse::Written { path, size: 0 })
            }
            DriveRequest::Rm { path, recursive } => self.drive_rm(&path, recursive),
            DriveRequest::Status { ws } => self.drive_status(&ws),
            DriveRequest::Commit { ws, message } => self.drive_commit(&ws, message),
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

    fn drive_create(&self, name: &str) -> Result<DriveResponse, WireError> {
        if !valid_workspace(name) {
            return Err(err(ErrorCode::InvalidInput, format!("工作区名只能是小写字母、数字和连字符，1 到 32 个字符：{name}")));
        }
        let mut state = self.drive_state.borrow_mut();
        if state.workspaces.contains_key(name) {
            return Err(err(ErrorCode::InvalidInput, format!("已经有工作区 {name}")));
        }
        let work = tree::empty(&mut Cas(&self.system));
        state.workspaces.insert(name.to_string(), WsState { head: None, work });
        save(&self.system, KEY_DRIVE, &*state).map_err(internal)?;
        Ok(DriveResponse::Created { ws: name.to_string() })
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

    fn drive_list(&self) -> Result<DriveResponse, WireError> {
        let all: Vec<(String, WsState)> = self.drive_state.borrow().workspaces.clone().into_iter().collect();
        let mut out = Vec::new();
        for (name, ws) in all {
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
        let mut next = ws.head;
        while let Some(h) = next {
            let c = self.commit(h)?;
            if h.to_string().starts_with(&prefix) {
                return Ok(c);
            }
            next = c.parents.first().copied();
        }
        Err(err(ErrorCode::NotFound, format!("没有提交 {prefix}")))
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
        if server.tx.send(CtlResponse::Ok).await.is_err() {
            return;
        }
        for span in manifest.spans(offset, length.unwrap_or(u64::MAX)) {
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

    fn drive_commit(&self, name: &str, message: String) -> Result<DriveResponse, WireError> {
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
        Ok(DriveResponse::Committed { commit: h.to_string() })
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
