//! Executable regression model: conflict copies must not overwrite existing files.
//! M4.2 / R50 promises that both divergent versions survive, never silently overwritten.
use nexus_drive_core::tree::{self, Entry, MemStore, Store};
use nexus_drive_core::{FileManifest, Hash};
use std::collections::BTreeMap;

fn file(store: &mut MemStore, text: &str) -> Entry {
    let bytes = text.as_bytes();
    store.put(bytes.to_vec());
    let manifest = store.put(FileManifest::from_chunks(&[bytes]).encode());
    Entry::File {
        manifest,
        size: bytes.len() as u64,
        exec: false,
    }
}

fn snapshot(store: &mut MemStore, files: &BTreeMap<String, Entry>) -> Hash {
    let mut root = tree::empty(store);
    for (path, entry) in files {
        let parts: Vec<_> = path.split('/').map(str::to_string).collect();
        root = tree::set(store, root, &parts, entry.clone()).unwrap();
    }
    root
}

#[test]
fn conflict_copy_preserves_existing_names_and_both_versions() {
    // covers: R50
    for directory in [false, true] {
        for count in 0..=3 {
            let mut store = MemStore::default();
            let mut shared = BTreeMap::new();
            for i in 0..count {
                let name = if i == 0 {
                    "report.conflict-laptop".to_string()
                } else {
                    format!("report.conflict-laptop-{i}")
                };
                let path = if directory {
                    format!("{name}/child")
                } else {
                    name
                };
                shared.insert(path, file(&mut store, &format!("existing-{i}")));
            }
            let old = file(&mut store, "base");
            let local = file(&mut store, "ours");
            let remote = file(&mut store, "theirs");
            let mut base = shared.clone();
            base.insert("report".to_string(), old);
            let mut ours = shared.clone();
            ours.insert("report".to_string(), local.clone());
            let mut theirs = shared.clone();
            theirs.insert("report".to_string(), remote.clone());
            let b = snapshot(&mut store, &base);
            let o = snapshot(&mut store, &ours);
            let t = snapshot(&mut store, &theirs);
            let (root, conflicts) = tree::merge(&mut store, Some(b), o, t, "laptop")
                .unwrap_or_else(|err| {
                    panic!("counterexample: directory={directory}, count={count}: {err:?}")
                });
            assert_eq!(conflicts, vec!["report".to_string()]);
            let actual = tree::files(&store, root).unwrap();
            // The specification requires the original local file and ALL unchanged
            // user files, plus exactly one copy of the remote version.
            let mut remaining = actual.clone();
            assert_eq!(remaining.remove("report"), Some(local));
            for (path, entry) in shared {
                assert_eq!(remaining.remove(&path), Some(entry), "lost existing {path}");
            }
            assert_eq!(
                remaining.len(),
                1,
                "counterexample: directory={directory}, count={count}"
            );
            let (name, entry) = remaining.iter().next().unwrap();
            assert!(name.starts_with("report.conflict-laptop"), "{name}");
            assert_eq!(entry, &remote);
            let (again, _) = tree::merge(&mut store, Some(b), o, t, "laptop").unwrap();
            assert_eq!(again, root, "merge must be deterministic");
        }
    }
}
