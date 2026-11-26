// covers: R41
//! N1 路由导入、静态审计与 LKG 回滚端到端场景测试（仿真 + 真实后端）。

use nexus_testkit::{scenario, World};

// TC-N1-05：规则原子导入保存 LKG 快照，并支持一键回滚恢复
scenario! {
    name: route_import_saves_lkg_and_rollback_restores,
    covers: "R41",
    nodes: ["laptop"],
    real: yes,
    run: |w: World| async move {
        w.cli_ok("laptop", &["init", "--name", "laptop"]).await;
        w.cli_ok("laptop", &["route", "add", "--domain", "old.test", "--reject"]).await;

        static INIT_FIXTURE: std::sync::Once = std::sync::Once::new();
        let clash_file = std::env::temp_dir().join("nexus-n1-clash-fixture.yaml");
        INIT_FIXTURE.call_once(|| {
            std::fs::write(
                &clash_file,
                "rules:\n  - DOMAIN,api.new.test,JP\n  - DOMAIN-SUFFIX,direct.test,DIRECT\n  - SCRIPT,bad,JP\n",
            )
            .unwrap();
        });

        // 先 dry-run 验证审计报告不改变生效规则
        let dry = w
            .cli_ok(
                "laptop",
                &[
                    "route",
                    "import",
                    clash_file.to_str().unwrap(),
                    "--via",
                    "jp-exit",
                    "--dry-run",
                ],
            )
            .await;
        assert_eq!(dry["dry_run"], true, "{dry}");
        assert_eq!(dry["imported"], 2, "{dry}");
        assert_eq!(dry["skipped"].as_array().unwrap().len(), 1, "{dry}");
        assert_eq!(w.cli_ok("laptop", &["route", "ls"]).await["rules"].as_array().unwrap().len(), 1);

        // 原子替换导入（旧规则保存为 LKG）
        let imp = w
            .cli_ok(
                "laptop",
                &[
                    "route",
                    "import",
                    clash_file.to_str().unwrap(),
                    "--via",
                    "jp-exit",
                    "--replace",
                ],
            )
            .await;
        assert_eq!(imp["total"], 2, "{imp}");
        let after_import = w.cli_ok("laptop", &["route", "ls"]).await;
        assert_eq!(after_import["rules"].as_array().unwrap().len(), 2, "{after_import}");
        assert_eq!(after_import["rules"][0]["match"], "exact:api.new.test");

        // 一键回滚恢复到 old.test -> reject
        w.cli_ok("laptop", &["route", "rollback"]).await;
        let after_rollback = w.cli_ok("laptop", &["route", "ls"]).await;
        assert_eq!(after_rollback["rules"].as_array().unwrap().len(), 1, "{after_rollback}");
        assert_eq!(after_rollback["rules"][0]["match"], "old.test");
        assert_eq!(after_rollback["rules"][0]["outbound"], "reject");
    },
}

// TC-N1-06：控制通道执行静态规则审计，检测被遮蔽的不可达规则
scenario! {
    name: route_audit_reports_shadowed_rules_via_ctl,
    covers: "R41",
    nodes: ["laptop"],
    real: yes,
    run: |w: World| async move {
        w.cli_ok("laptop", &["init", "--name", "laptop"]).await;
        w.cli_ok("laptop", &["route", "add", "--domain", "example.test", "--direct"]).await;
        w.cli_ok("laptop", &["route", "add", "--domain-exact", "api.example.test", "--reject"]).await;

        let audit = w.cli_ok("laptop", &["route", "audit"]).await;
        assert_eq!(audit["clean"], false, "{audit}");
        let issues = audit["issues"].as_array().unwrap();
        assert_eq!(issues.len(), 1, "{audit}");
        assert_eq!(issues[0]["kind"], "shadowed", "{audit}");
        assert_eq!(issues[0]["rule_index"], 2, "{audit}");
        assert_eq!(issues[0]["shadowed_by"], 1, "{audit}");
    },
}
