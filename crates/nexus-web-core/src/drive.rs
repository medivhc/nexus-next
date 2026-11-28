//! 网页里浏览工作区（M8.1，R70）：向存储设备要工作区的头和对象，在浏览器里拼出目录和文件。
//! 功能点见 docs/feature-points/M8.1.md。

use crate::node::WebNode;
use crate::unreachable;
use nexus_drive_core::tree::{Entry, Tree};
use nexus_drive_core::{Commit, FileManifest, Hash};
use nexus_fabric::{read_msg, read_response, write_msg, Connection, Proto, Transport};
use nexus_wire::{ErrorCode, Request, RequestHeader, Response, WireError};
use std::collections::BTreeMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// 一次最多要多少个对象。
const BATCH: usize = 64;

/// 一个能读的工作区，和保存它的存储设备。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DriveWorkspace {
    pub name: String,
    pub device: String,
}

/// 目录里的一项。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DriveEntry {
    pub name: String,
    /// `file`、`dir` 或 `symlink`。
    pub kind: String,
    /// 文件的大小；目录和链接是 0。
    pub size: u64,
    /// 链接的目标。
    pub target: Option<String>,
}

/// 读到的文件：总大小和开头的一部分。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DriveFile {
    pub size: u64,
    pub bytes: Vec<u8>,
    /// 没有读完。
    pub truncated: bool,
}

fn not_found(msg: String) -> WireError {
    WireError::new(ErrorCode::NotFound, msg)
}

fn invalid(msg: String) -> WireError {
    WireError::new(ErrorCode::InvalidInput, msg)
}

fn garbled(what: &str) -> WireError {
    WireError::new(ErrorCode::Internal, format!("存储设备给的{what}无法理解"))
}

/// 路径拆成段：前后和重复的 `/` 不影响；`.`、`..` 之类不合法。
fn parts(path: &str) -> Result<Vec<String>, WireError> {
    let parts: Vec<String> = path.split('/').filter(|p| !p.is_empty()).map(str::to_string).collect();
    match parts.iter().find(|p| !nexus_drive_core::path::valid_part(p)) {
        Some(bad) => Err(invalid(format!("路径里不能有 {bad}"))),
        None => Ok(parts),
    }
}

impl<T: Transport> WebNode<T> {
    /// 存储设备上它能读的工作区和头：（是否开着 serve，[(工作区, 头)]）。
    async fn drive_info(&self, device: &str) -> Result<(bool, Vec<(String, Option<[u8; 32]>)>), WireError> {
        match self.rpc(device, Request::DriveInfo).await? {
            Response::DriveInfo { serving, workspaces } => Ok((serving, workspaces)),
            other => Err(WireError::new(ErrorCode::Internal, format!("回应无法理解：{other:?}"))),
        }
    }

    /// 能读的工作区，按名字排序。同名的工作区在多台存储设备上时取名字靠前的设备。
    pub async fn drive_workspaces(&self) -> Result<Vec<DriveWorkspace>, WireError> {
        let mut found: BTreeMap<String, String> = BTreeMap::new();
        let mut devices: Vec<String> = self.devices().iter().map(|d| d.name.clone()).collect();
        devices.sort();
        for d in devices {
            // 连不上、没有授权查看的设备：跳过。
            let Ok((true, workspaces)) = self.drive_info(&d).await else { continue };
            for (ws, _) in workspaces {
                found.entry(ws).or_insert_with(|| d.clone());
            }
        }
        Ok(found.into_iter().map(|(name, device)| DriveWorkspace { name, device }).collect())
    }

    /// 工作区所在的存储设备和它的头（没有提交时为 `None`）。不在任何能读的存储设备上时：
    /// 存储设备不让读就是没有授权，否则是不存在。
    async fn drive_head(&self, ws: &str) -> Result<(String, Option<Hash>), WireError> {
        if !nexus_drive_core::path::valid_workspace(ws) {
            return Err(invalid(format!("工作区名不合法：{ws}")));
        }
        let mut devices: Vec<String> = self.devices().iter().map(|d| d.name.clone()).collect();
        devices.sort();
        let mut storage = Vec::new();
        for d in devices {
            let Ok((true, workspaces)) = self.drive_info(&d).await else { continue };
            if let Some((_, head)) = workspaces.into_iter().find(|(n, _)| n == ws) {
                return Ok((d, head.map(Hash)));
            }
            storage.push(d);
        }
        // 列表里没有：可能是没有授权（列表只给能读的）。问一下，存储设备会判定并记审计。
        for d in &storage {
            if let Err(e) = self.drive_get(d, ws, &[]).await {
                if e.code == ErrorCode::Denied {
                    return Err(WireError::new(ErrorCode::Denied, format!("没有读工作区 {ws} 的授权（{}）", e.message)));
                }
            }
        }
        Err(not_found(format!("能看到的存储设备上没有工作区 {ws}")))
    }

    /// 从存储设备取一批对象，按哈希校验。
    async fn drive_get(&self, device: &str, ws: &str, hashes: &[Hash]) -> Result<Vec<Vec<u8>>, WireError> {
        let to = self.device(device)?;
        let mut out = Vec::with_capacity(hashes.len());
        let batches: Vec<&[Hash]> = if hashes.is_empty() { vec![&[]] } else { hashes.chunks(BATCH).collect() };
        for batch in batches {
            let conn = self.transport().connect(to, Proto::Rpc).await.map_err(unreachable)?;
            let mut stream = conn.open_bi().await.map_err(unreachable)?;
            let body = Request::DriveGet { ws: ws.to_string(), hashes: batch.iter().map(|h| h.0).collect() };
            let header = RequestHeader::new(0, self.principal(), &body);
            write_msg(&mut stream, &header).await.map_err(unreachable)?;
            match read_response(&mut stream).await.map_err(unreachable)? {
                Response::Ok => {}
                Response::Error(e) => return Err(e),
                other => return Err(WireError::new(ErrorCode::Internal, format!("回应无法理解：{other:?}"))),
            }
            for h in batch {
                let obj: Option<Vec<u8>> = read_msg(&mut stream).await.map_err(unreachable)?;
                let bytes = obj.ok_or_else(|| not_found(format!("{device} 上缺少对象 {}", h.short())))?;
                if Hash::of(&bytes) != *h {
                    return Err(WireError::new(ErrorCode::Internal, format!("{device} 给的对象 {} 内容不对", h.short())));
                }
                out.push(bytes);
            }
            let _ = stream.shutdown().await;
            let mut sink = [0u8; 256];
            while matches!(stream.read(&mut sink).await, Ok(n) if n > 0) {}
        }
        Ok(out)
    }

    /// 一个对象（提交、目录、文件清单）：先看缓存。对象按哈希寻址，不会变。
    async fn drive_object(&self, device: &str, ws: &str, h: Hash) -> Result<Vec<u8>, WireError> {
        if let Some(b) = self.drive_cache().borrow().get(&h) {
            return Ok(b.clone());
        }
        let bytes = self.drive_get(device, ws, &[h]).await?.pop().ok_or_else(|| garbled("对象"))?;
        self.drive_cache().borrow_mut().insert(h, bytes.clone());
        Ok(bytes)
    }

    /// 路径上的那一项；根目录是 `Dir`。
    async fn drive_lookup(&self, ws: &str, path: &str) -> Result<(String, Entry), WireError> {
        let parts = parts(path)?;
        let (device, head) = self.drive_head(ws).await?;
        let Some(head) = head else {
            // 还没有提交：只有一个空的根目录。
            return match parts.is_empty() {
                true => Ok((device, Entry::Dir { tree: Hash([0; 32]) })),
                false => Err(not_found(format!("{ws}:/{} 不存在", parts.join("/")))),
            };
        };
        let commit = Commit::decode(&self.drive_object(&device, ws, head).await?).ok_or_else(|| garbled("提交"))?;
        let mut cur = Entry::Dir { tree: commit.tree };
        for (i, p) in parts.iter().enumerate() {
            let Entry::Dir { tree } = cur else {
                return Err(not_found(format!("{ws}:/{} 不是目录", parts[..i].join("/"))));
            };
            let t = Tree::decode(&self.drive_object(&device, ws, tree).await?).ok_or_else(|| garbled("目录"))?;
            cur = t.entries.get(p).cloned().ok_or_else(|| not_found(format!("{ws}:/{} 不存在", parts[..=i].join("/"))))?;
        }
        Ok((device, cur))
    }

    /// 列出目录：目录在前，然后按名字。
    pub async fn drive_list(&self, ws: &str, path: &str) -> Result<Vec<DriveEntry>, WireError> {
        let (device, entry) = self.drive_lookup(ws, path).await?;
        let Entry::Dir { tree } = entry else {
            return Err(invalid(format!("{ws}:/{} 是文件，不是目录", path.trim_matches('/'))));
        };
        if tree == Hash([0; 32]) {
            return Ok(Vec::new());
        }
        let t = Tree::decode(&self.drive_object(&device, ws, tree).await?).ok_or_else(|| garbled("目录"))?;
        let mut out: Vec<DriveEntry> = t
            .entries
            .into_iter()
            .map(|(name, e)| match e {
                Entry::File { size, .. } => DriveEntry { name, kind: "file".into(), size, target: None },
                Entry::Dir { .. } => DriveEntry { name, kind: "dir".into(), size: 0, target: None },
                Entry::Symlink { target } => DriveEntry { name, kind: "symlink".into(), size: 0, target: Some(target) },
            })
            .collect();
        out.sort_by(|a, b| (a.kind != "dir", &a.name).cmp(&(b.kind != "dir", &b.name)));
        Ok(out)
    }

    /// 读文件开头的最多 `max` 字节。
    pub async fn drive_read(&self, ws: &str, path: &str, max: u64) -> Result<DriveFile, WireError> {
        let (device, entry) = self.drive_lookup(ws, path).await?;
        let manifest = match entry {
            Entry::File { manifest, .. } => manifest,
            Entry::Dir { .. } => return Err(invalid(format!("{ws}:/{} 是目录，不是文件", path.trim_matches('/')))),
            Entry::Symlink { target } => return Err(invalid(format!("{ws}:/{} 是链接（指向 {target}）", path.trim_matches('/')))),
        };
        let m = FileManifest::decode(&self.drive_object(&device, ws, manifest).await?).ok_or_else(|| garbled("文件清单"))?;
        let spans = m.spans(0, max);
        let hashes: Vec<Hash> = spans.iter().map(|s| s.hash).collect();
        let chunks = self.drive_get(&device, ws, &hashes).await?;
        let mut bytes = Vec::new();
        for (s, c) in spans.iter().zip(&chunks) {
            let part = c.get(s.from as usize..s.to as usize).ok_or_else(|| garbled("块"))?;
            bytes.extend_from_slice(part);
        }
        let truncated = (bytes.len() as u64) < m.size;
        Ok(DriveFile { size: m.size, bytes, truncated })
    }
}
