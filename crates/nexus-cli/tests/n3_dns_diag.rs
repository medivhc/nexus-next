// covers: R43, R105, R106
//! N3 CLI 端到端测试（TC-N3-09 / TC-N3-10，覆盖仿真与真实后端）。

use nexus_testkit::{scenario, World};

scenario! {
    name: dns_rules_add_explain_list_rm_and_flow_ls,
    covers: "R43, R105, R106",
    nodes: ["laptop"],
    real: yes,
    run: |w: World| async move {
        w.cli_ok("laptop", &["init", "--name", "laptop"]).await;

        w.cli_ok("laptop", &["dns", "add", "--id", "block-ads", "--domain", "ads.test", "--block"]).await;
        w.cli_ok("laptop", &["dns", "add", "--id", "allow-ok", "--domain-exact", "ok.ads.test", "--allow"]).await;
        w.cli_ok("laptop", &["dns", "add", "--id", "dev-api", "--domain-exact", "api.dev.test", "--override", "10.0.0.5", "--ttl", "30"]).await;

        let list = w.cli_ok("laptop", &["dns", "ls"]).await;
        assert_eq!(list["rules"].as_array().unwrap().len(), 3, "{list}");

        let exp_allow = w.cli_ok("laptop", &["dns", "explain", "ok.ads.test"]).await;
        assert_eq!(exp_allow["result"]["Allowed"]["rule_id"], "allow-ok", "{exp_allow}");

        let exp_block = w.cli_ok("laptop", &["dns", "explain", "track.ads.test"]).await;
        assert_eq!(exp_block["result"]["BlockedNxdomain"]["rule_id"], "block-ads", "{exp_block}");

        let exp_ov = w.cli_ok("laptop", &["dns", "explain", "api.dev.test"]).await;
        assert_eq!(exp_ov["result"]["Overridden"]["rule_id"], "dev-api", "{exp_ov}");

        w.cli_ok("laptop", &["dns", "rm", "block-ads"]).await;
        let list2 = w.cli_ok("laptop", &["dns", "ls"]).await;
        assert_eq!(list2["rules"].as_array().unwrap().len(), 2, "{list2}");

        let flows = w.cli_ok("laptop", &["flow", "ls", "--limit", "10"]).await;
        assert!(flows["events"].is_array(), "{flows}");
    },
}
