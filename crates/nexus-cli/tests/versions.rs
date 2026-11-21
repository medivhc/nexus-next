//! 新旧版本互通的验收场景（M7.1，R65）。功能点见 docs/feature-points/M7.1.md。
//! 真正的新旧二进制混跑在 tools/compat_check.py。

use nexus_testkit::{scenario, World};

scenario! {
    name: ls_shows_each_devices_version,
    covers: "R65",
    nodes: ["laptop", "home"],
    real: yes,
    run: |w: World| async move {
        w.network("laptop", &["home"]).await;
        // FP-7.1-04
        let ls = w.cli_ok("laptop", &["ls"]).await;
        for d in ls["devices"].as_array().unwrap() {
            assert_eq!(d["version"], env!("CARGO_PKG_VERSION"), "{d}");
            assert_eq!(d["version_differs"], false, "{d}");
        }
        let text = w.cli("laptop", &["ls"]).await;
        assert!(!text.stdout.contains("和本机不同"), "{}", text.stdout);
        assert_eq!(w.cli_ok("laptop", &["status", "home"]).await["version"], env!("CARGO_PKG_VERSION"));
    },
}
