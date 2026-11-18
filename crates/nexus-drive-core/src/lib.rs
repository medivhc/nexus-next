//! Drive 的内容模型（纯逻辑）。
//!
//! 文件按内容切成块（FastCDC），块、文件清单、目录树和提交都按内容寻址（BLAKE3）。
//! 这里不碰系统：存储由调用方通过 [`tree::Store`] 提供。
#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use std::fmt;

/// 内容的身份：BLAKE3 哈希。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Hash(pub [u8; 32]);

impl Hash {
    pub fn of(bytes: &[u8]) -> Self {
        Self(*blake3::hash(bytes).as_bytes())
    }

    /// 显示用的前 12 个十六进制字符。
    pub fn short(&self) -> String {
        self.to_string()[..12].to_string()
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hash({})", self.short())
    }
}

impl std::str::FromStr for Hash {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, ()> {
        if s.len() != 64 {
            return Err(());
        }
        let mut out = [0u8; 32];
        for (i, b) in out.iter_mut().enumerate() {
            *b = u8::from_str_radix(s.get(2 * i..2 * i + 2).ok_or(())?, 16).map_err(|_| ())?;
        }
        Ok(Self(out))
    }
}

/// 编码后的对象带一个类型字节，免得把一种对象当成另一种解码。
fn encode_tagged<T: Serialize>(tag: u8, v: &T) -> Vec<u8> {
    let mut out = vec![tag];
    out.extend(postcard::to_allocvec(v).expect("内存里的结构总能编码"));
    out
}

fn decode_tagged<'a, T: Deserialize<'a>>(tag: u8, bytes: &'a [u8]) -> Option<T> {
    match bytes.split_first() {
        Some((&t, rest)) if t == tag => postcard::from_bytes(rest).ok(),
        _ => None,
    }
}

const TAG_MANIFEST: u8 = b'M';
const TAG_TREE: u8 = b'T';
const TAG_COMMIT: u8 = b'C';

/// 分块。
pub mod chunk {
    /// 块的最小、平均、最大长度（FastCDC）。小于最小长度的文件就是一块。
    pub const MIN: usize = 16 * 1024;
    pub const AVG: usize = 64 * 1024;
    pub const MAX: usize = 256 * 1024;

    /// 一次性切分。
    pub fn chunk_all(data: &[u8]) -> Vec<&[u8]> {
        if data.is_empty() {
            return Vec::new();
        }
        fastcdc::v2020::FastCDC::new(data, MIN as u32, AVG as u32, MAX as u32)
            .map(|c| &data[c.offset..c.offset + c.length])
            .collect()
    }

    /// 流式切分：数据分几次到达，结果和一次性切分相同。
    /// 缓冲里的最后一块可能只是因为数据还没到齐才断开，所以留到下次或 `finish`。
    #[derive(Default)]
    pub struct Chunker {
        buf: Vec<u8>,
    }

    impl Chunker {
        pub fn push(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
            self.buf.extend_from_slice(data);
            if self.buf.len() < 2 * MAX {
                return Vec::new();
            }
            let chunks = chunk_all(&self.buf);
            let keep = chunks.last().map_or(0, |c| c.len());
            let done = self.buf.len() - keep;
            let out = chunks[..chunks.len() - 1].iter().map(|c| c.to_vec()).collect();
            self.buf.drain(..done);
            out
        }

        pub fn finish(self) -> Vec<Vec<u8>> {
            chunk_all(&self.buf).into_iter().map(<[u8]>::to_vec).collect()
        }
    }
}

/// 文件清单里的一块。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkRef {
    pub hash: Hash,
    pub len: u32,
}

/// 一个文件：按顺序的块。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileManifest {
    pub size: u64,
    pub chunks: Vec<ChunkRef>,
}

/// 读一个范围要用到的一块的一部分：第 `index` 块的 `[from, to)`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub index: usize,
    pub hash: Hash,
    pub from: u32,
    pub to: u32,
}

impl FileManifest {
    pub fn from_chunks<C: AsRef<[u8]>>(chunks: &[C]) -> Self {
        let chunks: Vec<ChunkRef> =
            chunks.iter().map(|c| ChunkRef { hash: Hash::of(c.as_ref()), len: c.as_ref().len() as u32 }).collect();
        Self { size: chunks.iter().map(|c| c.len as u64).sum(), chunks }
    }

    pub fn encode(&self) -> Vec<u8> {
        encode_tagged(TAG_MANIFEST, self)
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        decode_tagged(TAG_MANIFEST, bytes)
    }

    /// 读 `[offset, offset + len)` 要用到哪些块的哪些部分；越过末尾的部分忽略。
    pub fn spans(&self, offset: u64, len: u64) -> Vec<Span> {
        let end = offset.saturating_add(len).min(self.size);
        let mut out = Vec::new();
        let mut start = 0u64;
        for (index, c) in self.chunks.iter().enumerate() {
            let stop = start + c.len as u64;
            if stop > offset && start < end {
                let from = offset.saturating_sub(start) as u32;
                let to = (end.min(stop) - start) as u32;
                out.push(Span { index, hash: c.hash, from, to });
            }
            if stop >= end {
                break;
            }
            start = stop;
        }
        out
    }
}

/// 一次提交：不可变的快照。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Commit {
    pub tree: Hash,
    pub parents: Vec<Hash>,
    /// 在哪台设备上提交的。
    pub device: String,
    pub time_ms: u64,
    pub message: String,
}

impl Commit {
    pub fn encode(&self) -> Vec<u8> {
        encode_tagged(TAG_COMMIT, self)
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        decode_tagged(TAG_COMMIT, bytes)
    }
}

/// 提交历史。
pub mod history {
    use super::{Commit, Hash};
    use std::collections::{BTreeSet, VecDeque};

    /// `a` 相对 `b` 的位置。
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum Relation {
        Same,
        /// `a` 是 `b` 的后代（`b` 可以快进到 `a`）。
        Ahead,
        /// `a` 是 `b` 的祖先。
        Behind,
        /// 两边各有对方没有的提交；`base` 是最近的共同祖先（没有共同历史时为空）。
        Diverged { base: Option<Hash> },
    }

    /// `head` 和它的全部祖先。缺少某个提交时返回它的哈希。
    pub fn ancestors(get: &impl Fn(&Hash) -> Option<Commit>, head: Hash) -> Result<BTreeSet<Hash>, Hash> {
        let mut seen = BTreeSet::new();
        let mut queue = VecDeque::from([head]);
        while let Some(h) = queue.pop_front() {
            if !seen.insert(h) {
                continue;
            }
            let c = get(&h).ok_or(h)?;
            queue.extend(c.parents);
        }
        Ok(seen)
    }

    pub fn relation(get: &impl Fn(&Hash) -> Option<Commit>, a: Hash, b: Hash) -> Result<Relation, Hash> {
        if a == b {
            return Ok(Relation::Same);
        }
        let of_a = ancestors(get, a)?;
        if of_a.contains(&b) {
            return Ok(Relation::Ahead);
        }
        let of_b = ancestors(get, b)?;
        if of_b.contains(&a) {
            return Ok(Relation::Behind);
        }
        // 最近的共同祖先：从 a 出发按广度优先找到的第一个 b 的祖先。
        let mut queue = VecDeque::from([a]);
        let mut seen = BTreeSet::new();
        while let Some(h) = queue.pop_front() {
            if !seen.insert(h) {
                continue;
            }
            if of_b.contains(&h) {
                return Ok(Relation::Diverged { base: Some(h) });
            }
            queue.extend(get(&h).ok_or(h)?.parents);
        }
        Ok(Relation::Diverged { base: None })
    }
}

/// 路径：`<工作区>:/<路径>`。
pub mod path {
    use std::fmt;

    /// 工作区名：小写字母、数字、连字符，1 到 32 个字符，不以连字符开头。
    pub fn valid_workspace(name: &str) -> bool {
        (1..=32).contains(&name.len())
            && !name.starts_with('-')
            && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    }

    /// 路径里的一段：非空，不是 `.` 或 `..`，不含 `/` 和 NUL，至多 255 字节。
    pub fn valid_part(part: &str) -> bool {
        !part.is_empty() && part != "." && part != ".." && part.len() <= 255 && !part.contains(['/', '\0'])
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct DrivePath {
        pub ws: String,
        pub parts: Vec<String>,
    }

    impl std::str::FromStr for DrivePath {
        type Err = String;
        fn from_str(s: &str) -> Result<Self, String> {
            let (ws, rest) = s.split_once(':').ok_or_else(|| format!("路径应当写成 <工作区>:/<路径>：{s}"))?;
            if !valid_workspace(ws) {
                return Err(format!("工作区名只能是小写字母、数字和连字符，1 到 32 个字符：{ws}"));
            }
            let parts: Vec<String> = rest.split('/').filter(|p| !p.is_empty()).map(str::to_string).collect();
            if let Some(bad) = parts.iter().find(|p| !valid_part(p)) {
                return Err(format!("路径里不能有 {bad}"));
            }
            Ok(Self { ws: ws.to_string(), parts })
        }
    }

    impl fmt::Display for DrivePath {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "{}:/{}", self.ws, self.parts.join("/"))
        }
    }
}

/// 目录树的编辑和比较。
pub mod tree {
    use super::{decode_tagged, encode_tagged, Hash, TAG_TREE};
    use serde::{Deserialize, Serialize};
    use std::collections::BTreeMap;

    /// 目录里的一项。
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub enum Entry {
        File { manifest: Hash, size: u64, exec: bool },
        Dir { tree: Hash },
        Symlink { target: String },
    }

    /// 一个目录。
    #[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
    pub struct Tree {
        pub entries: BTreeMap<String, Entry>,
    }

    impl Tree {
        pub fn encode(&self) -> Vec<u8> {
            encode_tagged(TAG_TREE, self)
        }

        pub fn decode(bytes: &[u8]) -> Option<Self> {
            decode_tagged(TAG_TREE, bytes)
        }
    }

    /// 按内容寻址的对象存储。
    pub trait Store {
        fn get(&self, hash: &Hash) -> Option<Vec<u8>>;
        /// 存入并返回它的哈希；已有时什么也不做。
        fn put(&mut self, bytes: Vec<u8>) -> Hash;
    }

    /// 内存里的存储（测试用）。
    #[derive(Default)]
    pub struct MemStore(pub BTreeMap<Hash, Vec<u8>>);

    impl MemStore {
        pub fn bytes(&self) -> usize {
            self.0.values().map(Vec::len).sum()
        }
    }

    impl Store for MemStore {
        fn get(&self, hash: &Hash) -> Option<Vec<u8>> {
            self.0.get(hash).cloned()
        }
        fn put(&mut self, bytes: Vec<u8>) -> Hash {
            let h = Hash::of(&bytes);
            self.0.entry(h).or_insert(bytes);
            h
        }
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum DriveError {
        NotFound,
        NotADirectory,
        IsADirectory,
        /// 本机没有这个对象（还没取到，或存储损坏）。
        Missing(Hash),
    }

    pub fn empty(store: &mut impl Store) -> Hash {
        store.put(Tree::default().encode())
    }

    pub fn load(store: &(impl Store + ?Sized), hash: Hash) -> Result<Tree, DriveError> {
        store.get(&hash).and_then(|b| Tree::decode(&b)).ok_or(DriveError::Missing(hash))
    }

    /// 找到路径上的那一项；根目录本身用空路径表示，返回 `Dir`。
    pub fn lookup(store: &(impl Store + ?Sized), root: Hash, parts: &[String]) -> Result<Option<Entry>, DriveError> {
        let mut cur = Entry::Dir { tree: root };
        for p in parts {
            let Entry::Dir { tree } = cur else { return Ok(None) };
            match load(store, tree)?.entries.get(p) {
                Some(e) => cur = e.clone(),
                None => return Ok(None),
            }
        }
        Ok(Some(cur))
    }

    /// 把路径上的项设成 `entry`，中间的目录自动建立，返回新的根。
    pub fn set(store: &mut impl Store, root: Hash, parts: &[String], entry: Entry) -> Result<Hash, DriveError> {
        let Some((first, rest)) = parts.split_first() else {
            return match entry {
                Entry::Dir { tree } => Ok(tree),
                _ => Err(DriveError::IsADirectory),
            };
        };
        let mut t = load(store, root)?;
        let child = if rest.is_empty() {
            entry
        } else {
            let sub = match t.entries.get(first) {
                Some(Entry::Dir { tree }) => *tree,
                Some(_) => return Err(DriveError::NotADirectory),
                None => empty(store),
            };
            Entry::Dir { tree: set(store, sub, rest, entry)? }
        };
        t.entries.insert(first.clone(), child);
        Ok(store.put(t.encode()))
    }

    /// 删除路径上的项，返回新的根。目录要 `recursive`。
    pub fn remove(store: &mut impl Store, root: Hash, parts: &[String], recursive: bool) -> Result<Hash, DriveError> {
        let Some((first, rest)) = parts.split_first() else { return Err(DriveError::IsADirectory) };
        let mut t = load(store, root)?;
        match (t.entries.get(first).cloned(), rest.is_empty()) {
            (None, _) => return Err(DriveError::NotFound),
            (Some(Entry::Dir { .. }), true) if !recursive => return Err(DriveError::IsADirectory),
            (Some(_), true) => {
                t.entries.remove(first);
            }
            (Some(Entry::Dir { tree }), false) => {
                let sub = remove(store, tree, rest, recursive)?;
                t.entries.insert(first.clone(), Entry::Dir { tree: sub });
            }
            (Some(_), false) => return Err(DriveError::NotFound),
        }
        Ok(store.put(t.encode()))
    }

    /// 全部文件和符号链接（不含目录），路径用 `/` 连接。
    pub fn files(store: &(impl Store + ?Sized), root: Hash) -> Result<BTreeMap<String, Entry>, DriveError> {
        let mut out = BTreeMap::new();
        collect(store, root, "", &mut out)?;
        Ok(out)
    }

    fn collect(store: &(impl Store + ?Sized), root: Hash, prefix: &str, out: &mut BTreeMap<String, Entry>) -> Result<(), DriveError> {
        for (name, e) in load(store, root)?.entries {
            let path = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
            match e {
                Entry::Dir { tree } => collect(store, tree, &path, out)?,
                e => {
                    out.insert(path, e);
                }
            }
        }
        Ok(())
    }

    /// 按文件三方合并：只有一边改的取那一边；两边改得不同时保留 `ours`，`theirs` 的版本另存为
    /// `<路径>.conflict-<label>`（一边删除、另一边修改时同样）。返回合并后的根和冲突的路径。
    /// 合并结果里不保留空目录。
    pub fn merge(store: &mut impl Store, base: Option<Hash>, ours: Hash, theirs: Hash, label: &str) -> Result<(Hash, Vec<String>), DriveError> {
        let b = base.map(|h| files(store, h)).transpose()?.unwrap_or_default();
        let o = files(store, ours)?;
        let t = files(store, theirs)?;
        let paths: std::collections::BTreeSet<&String> = b.keys().chain(o.keys()).chain(t.keys()).collect();
        let mut merged: BTreeMap<String, Entry> = BTreeMap::new();
        let mut conflicts = Vec::new();
        for p in paths {
            let (eb, eo, et) = (b.get(p), o.get(p), t.get(p));
            let pick = if eo == et || et == eb {
                eo
            } else if eo == eb {
                et
            } else {
                conflicts.push(p.clone());
                if let Some(e) = et {
                    merged.insert(format!("{p}.conflict-{label}"), e.clone());
                }
                // 本机删除、对方修改：保留对方的修改（另存），本机这边没有文件。
                eo
            };
            if let Some(e) = pick {
                merged.insert(p.clone(), e.clone());
            }
        }
        let mut root = empty(store);
        for (p, e) in merged {
            let parts: Vec<String> = p.split('/').map(str::to_string).collect();
            root = set(store, root, &parts, e)?;
        }
        Ok((root, conflicts))
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
    pub enum ChangeKind {
        Added,
        Modified,
        Deleted,
    }

    /// 文件或符号链接的一处改动（目录本身不单独列出）。
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct Change {
        pub kind: ChangeKind,
        pub path: String,
    }

    /// 从 `old` 到 `new` 的改动，按路径排序。
    pub fn diff(store: &(impl Store + ?Sized), old: Option<Hash>, new: Hash) -> Result<Vec<Change>, DriveError> {
        let mut out = Vec::new();
        diff_dir(store, old, Some(new), "", &mut out)?;
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    fn diff_dir(store: &(impl Store + ?Sized), old: Option<Hash>, new: Option<Hash>, prefix: &str, out: &mut Vec<Change>) -> Result<(), DriveError> {
        if old == new {
            return Ok(());
        }
        let a = old.map(|h| load(store, h)).transpose()?.unwrap_or_default();
        let b = new.map(|h| load(store, h)).transpose()?.unwrap_or_default();
        let names: std::collections::BTreeSet<&String> = a.entries.keys().chain(b.entries.keys()).collect();
        for name in names {
            let path = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
            let (x, y) = (a.entries.get(name), b.entries.get(name));
            let dir = |e: Option<&Entry>| match e {
                Some(Entry::Dir { tree }) => Some(*tree),
                _ => None,
            };
            fn leaf(e: Option<&Entry>) -> Option<&Entry> {
                e.filter(|e| !matches!(e, Entry::Dir { .. }))
            }
            // 目录里的变化递归比较；文件和符号链接直接比较。
            diff_dir(store, dir(x), dir(y), &path, out)?;
            match (leaf(x), leaf(y)) {
                (None, Some(_)) => out.push(Change { kind: ChangeKind::Added, path }),
                (Some(_), None) => out.push(Change { kind: ChangeKind::Deleted, path }),
                (Some(p), Some(q)) if p != q => out.push(Change { kind: ChangeKind::Modified, path }),
                _ => {}
            }
        }
        Ok(())
    }
}
