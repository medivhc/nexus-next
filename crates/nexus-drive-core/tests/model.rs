//! Drive 的内容模型（M4.1）。功能点见 docs/feature-points/M4.1.md。
// covers: R47, R48, R49

use nexus_drive_core::chunk::{self, Chunker};
use nexus_drive_core::path::DrivePath;
use nexus_drive_core::tree::{self, Change, ChangeKind, DriveError, Entry, MemStore, Store};
use nexus_drive_core::{FileManifest, Hash};
use proptest::prelude::*;

/// 可重复的伪随机数据。
fn data(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

#[test]
fn chunks_respect_the_size_limits_and_cover_the_data() {
    let d = data(3 * 1024 * 1024, 1);
    let chunks = chunk::chunk_all(&d);
    assert!(chunks.len() > 10, "3 MiB 应当切成多块：{}", chunks.len());
    assert_eq!(chunks.concat(), d);
    for c in &chunks[..chunks.len() - 1] {
        assert!(
            c.len() >= chunk::MIN && c.len() <= chunk::MAX,
            "{}",
            c.len()
        );
    }
    // 小文件是一块；空文件没有块。
    assert_eq!(chunk::chunk_all(b"hello").len(), 1);
    assert!(chunk::chunk_all(b"").is_empty());
}

#[test]
fn an_edit_in_the_middle_keeps_most_chunks() {
    let a = data(2 * 1024 * 1024, 7);
    let mut b = a.clone();
    b.splice(1_000_000..1_000_000, b"inserted bytes".iter().copied());
    let ha: std::collections::HashSet<_> =
        chunk::chunk_all(&a).iter().map(|c| Hash::of(c)).collect();
    let hb: Vec<_> = chunk::chunk_all(&b).iter().map(|c| Hash::of(c)).collect();
    let same = hb.iter().filter(|h| ha.contains(h)).count();
    assert!(
        same + 3 >= hb.len(),
        "插入几个字节只应影响附近的块：{same}/{}",
        hb.len()
    );
}

#[test]
fn hashes_print_and_parse_as_hex() {
    let h = Hash::of(b"abc");
    let s = h.to_string();
    assert_eq!(s.len(), 64);
    assert_eq!(s.parse::<Hash>(), Ok(h));
    assert_eq!(&h.short(), &s[..12]);
    assert!("xyz".parse::<Hash>().is_err());
}

#[test]
fn manifests_map_ranges_to_chunks() {
    let d = data(700_000, 3);
    let chunks = chunk::chunk_all(&d);
    let m = FileManifest::from_chunks(&chunks);
    assert_eq!(m.size, d.len() as u64);
    let read = |off: u64, len: u64| -> Vec<u8> {
        m.spans(off, len)
            .iter()
            .flat_map(|s| chunks[s.index][s.from as usize..s.to as usize].to_vec())
            .collect()
    };
    assert_eq!(read(0, 10), d[..10]);
    assert_eq!(read(123_456, 300_000), d[123_456..423_456]);
    assert_eq!(read(699_990, 100), d[699_990..], "越过末尾时读到末尾为止");
    assert!(read(800_000, 10).is_empty());
    // 恰好在块边界上。
    let b = chunks[0].len() as u64;
    assert_eq!(read(b, 5), d[b as usize..b as usize + 5]);
    assert_eq!(m.spans(b, 5).len(), 1);
    assert_eq!(FileManifest::decode(&m.encode()), Some(m));
}

#[test]
fn paths_are_parsed() {
    let p: DrivePath = "work:/src/main.rs".parse().unwrap();
    assert_eq!(
        (p.ws.as_str(), p.parts.clone()),
        ("work", vec!["src".to_string(), "main.rs".to_string()])
    );
    assert_eq!(
        "work:".parse::<DrivePath>().unwrap().parts,
        Vec::<String>::new()
    );
    assert_eq!(
        "work:/".parse::<DrivePath>().unwrap().parts,
        Vec::<String>::new()
    );
    assert_eq!(
        "work:/a//b/".parse::<DrivePath>().unwrap().parts,
        vec!["a", "b"]
    );
    assert_eq!(p.to_string(), "work:/src/main.rs");
    for bad in [
        "nocolon",
        "Work:/a",
        ":/a",
        "work:/a/../b",
        "work:/./a",
        "toolongtoolongtoolongtoolongtoolong:/",
        "w_s:/",
    ] {
        assert!(bad.parse::<DrivePath>().is_err(), "{bad}");
    }
    assert!(nexus_drive_core::path::valid_workspace("my-work-2"));
}

fn file(store: &mut MemStore, content: &[u8]) -> Entry {
    let chunks = chunk::chunk_all(content);
    for c in &chunks {
        store.put(c.to_vec());
    }
    let m = FileManifest::from_chunks(&chunks);
    Entry::File {
        manifest: store.put(m.encode()),
        size: m.size,
        exec: false,
    }
}

fn parts(p: &str) -> Vec<String> {
    p.split('/').map(str::to_string).collect()
}

#[test]
fn trees_are_edited_copy_on_write() {
    let mut s = MemStore::default();
    let empty = tree::empty(&mut s);
    let f = file(&mut s, b"fn main() {}");
    let r1 = tree::set(&mut s, empty, &parts("src/main.rs"), f.clone()).unwrap();
    assert_eq!(
        tree::lookup(&s, r1, &parts("src/main.rs")).unwrap(),
        Some(f.clone())
    );
    assert!(matches!(
        tree::lookup(&s, r1, &parts("src")).unwrap(),
        Some(Entry::Dir { .. })
    ));
    assert_eq!(tree::lookup(&s, r1, &parts("nope")).unwrap(), None);
    // 旧的根不受影响。
    assert_eq!(tree::lookup(&s, empty, &parts("src")).unwrap(), None);
    // 同样的内容同样的根（按内容寻址）。
    let again = tree::set(&mut s, empty, &parts("src/main.rs"), f.clone()).unwrap();
    assert_eq!(again, r1);
    // 文件下面不能再建东西。
    assert_eq!(
        tree::set(&mut s, r1, &parts("src/main.rs/x"), f.clone()),
        Err(DriveError::NotADirectory)
    );
    // 删除
    assert_eq!(
        tree::remove(&mut s, r1, &parts("src/nope"), false),
        Err(DriveError::NotFound)
    );
    assert_eq!(
        tree::remove(&mut s, r1, &parts("src"), false),
        Err(DriveError::IsADirectory)
    );
    let r2 = tree::remove(&mut s, r1, &parts("src"), true).unwrap();
    assert_eq!(r2, empty, "删光以后回到空树");
    let r3 = tree::remove(&mut s, r1, &parts("src/main.rs"), false).unwrap();
    assert!(
        matches!(
            tree::lookup(&s, r3, &parts("src")).unwrap(),
            Some(Entry::Dir { .. })
        ),
        "空目录保留"
    );
}

#[test]
fn diffs_list_changed_files() {
    let mut s = MemStore::default();
    let empty = tree::empty(&mut s);
    let (a, b) = (file(&mut s, b"a"), file(&mut s, b"b"));
    let r1 = tree::set(&mut s, empty, &parts("x/a.txt"), a.clone()).unwrap();
    let r1 = tree::set(&mut s, r1, &parts("keep.txt"), a.clone()).unwrap();
    let r2 = tree::set(&mut s, r1, &parts("x/a.txt"), b.clone()).unwrap();
    let r2 = tree::set(&mut s, r2, &parts("y/new.txt"), b.clone()).unwrap();
    let r2 = tree::remove(&mut s, r2, &parts("keep.txt"), false).unwrap();
    let d = tree::diff(&s, Some(r1), r2).unwrap();
    assert_eq!(
        d,
        vec![
            Change {
                kind: ChangeKind::Deleted,
                path: "keep.txt".into()
            },
            Change {
                kind: ChangeKind::Modified,
                path: "x/a.txt".into()
            },
            Change {
                kind: ChangeKind::Added,
                path: "y/new.txt".into()
            },
        ]
    );
    assert_eq!(tree::diff(&s, Some(r2), r2).unwrap(), vec![]);
    assert_eq!(
        tree::diff(&s, None, r1).unwrap().len(),
        2,
        "没有旧版本时全是新增"
    );
}

#[test]
fn identical_content_is_stored_once() {
    let mut s = MemStore::default();
    let big = data(1024 * 1024, 9);
    file(&mut s, &big);
    let before = s.bytes();
    file(&mut s, &big);
    assert_eq!(s.bytes(), before);
}

#[test]
fn commits_round_trip() {
    use nexus_drive_core::Commit;
    let c = Commit {
        tree: Hash::of(b"t"),
        parents: vec![Hash::of(b"p")],
        device: "laptop".into(),
        time_ms: 42,
        message: "first".into(),
    };
    assert_eq!(Commit::decode(&c.encode()), Some(c));
    assert_eq!(Commit::decode(b"garbage"), None);
}

proptest! {
    /// 流式分块（数据分几次到达）和一次性分块的结果相同。
    #[test]
    fn streaming_chunking_matches(len in 0usize..1_500_000, cuts in proptest::collection::vec(1usize..400_000, 0..8), seed in any::<u64>()) {
        let d = data(len, seed);
        let mut c = Chunker::default();
        let mut out: Vec<Vec<u8>> = Vec::new();
        let mut pos = 0;
        for cut in cuts {
            let end = (pos + cut).min(d.len());
            out.extend(c.push(&d[pos..end]));
            pos = end;
        }
        out.extend(c.push(&d[pos..]));
        out.extend(c.finish());
        let whole: Vec<Vec<u8>> = chunk::chunk_all(&d).into_iter().map(<[u8]>::to_vec).collect();
        prop_assert_eq!(out, whole);
    }

    #[test]
    fn spans_read_back_any_range(len in 0usize..600_000, off in 0u64..700_000, n in 0u64..400_000, seed in any::<u64>()) {
        let d = data(len, seed);
        let chunks = chunk::chunk_all(&d);
        let m = FileManifest::from_chunks(&chunks);
        let got: Vec<u8> = m.spans(off, n).iter().flat_map(|s| chunks[s.index][s.from as usize..s.to as usize].to_vec()).collect();
        let start = (off as usize).min(d.len());
        let end = ((off + n) as usize).min(d.len());
        prop_assert_eq!(got, d[start..end].to_vec());
    }
}
