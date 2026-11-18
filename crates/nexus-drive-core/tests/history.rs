//! 历史与合并（M4.2 的纯逻辑部分）。
// covers: R50

use nexus_drive_core::history::{self, Relation};
use nexus_drive_core::tree::{self, Entry, MemStore, Store};
use nexus_drive_core::{chunk, Commit, FileManifest, Hash};
use std::collections::BTreeMap;

fn file(s: &mut MemStore, content: &[u8]) -> Entry {
    let chunks = chunk::chunk_all(content);
    for c in &chunks {
        s.put(c.to_vec());
    }
    let m = FileManifest::from_chunks(&chunks);
    Entry::File { manifest: s.put(m.encode()), size: m.size, exec: false }
}

fn tree_of(s: &mut MemStore, files: &[(&str, &[u8])]) -> Hash {
    let mut root = tree::empty(s);
    for (p, c) in files {
        let e = file(s, c);
        root = tree::set(s, root, &p.split('/').map(str::to_string).collect::<Vec<_>>(), e).unwrap();
    }
    root
}

fn commit(s: &mut MemStore, tree: Hash, parents: Vec<Hash>, msg: &str) -> Hash {
    s.put(Commit { tree, parents, device: "d".into(), time_ms: 0, message: msg.into() }.encode())
}

fn get(s: &MemStore) -> impl Fn(&Hash) -> Option<Commit> + '_ {
    |h| s.get(h).and_then(|b| Commit::decode(&b))
}

#[test]
fn relations_between_heads() {
    let mut s = MemStore::default();
    let t = tree::empty(&mut s);
    let a = commit(&mut s, t, vec![], "a");
    let b = commit(&mut s, t, vec![a], "b");
    let c = commit(&mut s, t, vec![b], "c");
    let x = commit(&mut s, t, vec![a], "x");
    {
        let g = get(&s);
        assert_eq!(history::relation(&g, c, c), Ok(Relation::Same));
        assert_eq!(history::relation(&g, c, a), Ok(Relation::Ahead), "c 是 a 的后代");
        assert_eq!(history::relation(&g, a, c), Ok(Relation::Behind));
        assert_eq!(history::relation(&g, c, x), Ok(Relation::Diverged { base: Some(a) }));
    }
    let m = commit(&mut s, t, vec![c, x], "merge");
    {
        let g = get(&s);
        assert_eq!(history::relation(&g, m, x), Ok(Relation::Ahead), "合并提交是两边的后代");
        assert_eq!(history::relation(&g, m, c), Ok(Relation::Ahead));
    }
    // 历史不完整时报告缺少的提交。
    let missing = Hash::of(b"nowhere");
    let orphan = commit(&mut s, t, vec![missing], "orphan");
    let g = get(&s);
    assert_eq!(history::relation(&g, orphan, c), Err(missing));
}

#[test]
fn unrelated_histories_have_no_base() {
    let mut s = MemStore::default();
    let t = tree::empty(&mut s);
    let a = commit(&mut s, t, vec![], "a");
    let b = commit(&mut s, t, vec![], "b");
    assert_eq!(history::relation(&get(&s), a, b), Ok(Relation::Diverged { base: None }));
}

#[test]
fn files_merge_three_ways() {
    let mut s = MemStore::default();
    let base = tree_of(&mut s, &[("same.txt", b"s"), ("ours.txt", b"o0"), ("theirs.txt", b"t0"), ("both.txt", b"b0"), ("gone.txt", b"g")]);
    let ours = tree_of(&mut s, &[("same.txt", b"s"), ("ours.txt", b"o1"), ("theirs.txt", b"t0"), ("both.txt", b"b-ours"), ("new-ours.txt", b"n")]);
    let theirs = tree_of(&mut s, &[("same.txt", b"s"), ("ours.txt", b"o0"), ("theirs.txt", b"t1"), ("both.txt", b"b-theirs"), ("gone.txt", b"g"), ("dir/new.txt", b"d")]);
    let (merged, conflicts) = tree::merge(&mut s, Some(base), ours, theirs, "laptop").unwrap();
    let files: BTreeMap<String, Entry> = tree::files(&s, merged).unwrap();
    let content = |s: &MemStore, e: &Entry| -> Vec<u8> {
        let Entry::File { manifest, .. } = e else { panic!() };
        let m = FileManifest::decode(&s.get(manifest).unwrap()).unwrap();
        m.chunks.iter().flat_map(|c| s.get(&c.hash).unwrap()).collect()
    };
    let got: BTreeMap<String, Vec<u8>> = files.iter().map(|(p, e)| (p.clone(), content(&s, e))).collect();
    let want: BTreeMap<String, Vec<u8>> = [
        ("both.txt", &b"b-ours"[..]),
        ("both.txt.conflict-laptop", b"b-theirs"),
        ("dir/new.txt", b"d"),
        ("new-ours.txt", b"n"),
        ("ours.txt", b"o1"),
        ("same.txt", b"s"),
        ("theirs.txt", b"t1"),
    ]
    .iter()
    .map(|(p, c)| (p.to_string(), c.to_vec()))
    .collect();
    assert_eq!(got, want, "gone.txt 被本机删除、对方没改：删除生效");
    assert_eq!(conflicts, vec!["both.txt".to_string()]);
}

#[test]
fn delete_against_modify_keeps_the_modification_and_reports_it() {
    let mut s = MemStore::default();
    let base = tree_of(&mut s, &[("f.txt", b"0")]);
    let ours = tree_of(&mut s, &[]);
    let theirs = tree_of(&mut s, &[("f.txt", b"1")]);
    let (merged, conflicts) = tree::merge(&mut s, Some(base), ours, theirs, "desk").unwrap();
    assert!(tree::files(&s, merged).unwrap().contains_key("f.txt.conflict-desk"));
    assert_eq!(conflicts, vec!["f.txt".to_string()]);
}
