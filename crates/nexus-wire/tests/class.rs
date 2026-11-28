//! 流量分类（M3.5）：每条流按它承载的请求分成交互、普通、批量三类，发送时交互优先。

use nexus_wire::{ExecSpec, Request, TrafficClass};

#[test]
fn terminals_and_control_are_interactive_services_and_exits_are_bulk() {
    use TrafficClass::*;
    let size = nexus_wire::Size { cols: 80, rows: 24 };
    assert_eq!(
        Request::SessionAttach {
            name: "main".into(),
            create: true,
            size
        }
        .class(),
        Interactive
    );
    assert_eq!(Request::NodeInfo.class(), Interactive);
    assert_eq!(Request::RosterSync.class(), Interactive);
    assert_eq!(Request::SessionList.class(), Interactive);
    assert_eq!(Request::SvcList.class(), Interactive);
    let spec = ExecSpec {
        program: "true".into(),
        args: vec![],
        cwd: None,
        env: vec![],
        timeout_ms: None,
        max_output: 1,
    };
    assert_eq!(
        Request::Exec(spec).class(),
        Normal,
        "命令的输出可能很大，但通常短：介于两者之间"
    );
    assert_eq!(Request::SvcConnect { name: "web".into() }.class(), Bulk);
    assert_eq!(
        Request::ExitConnect {
            target: "a:1".into()
        }
        .class(),
        Bulk
    );
    assert_eq!(
        Request::ExitDatagram {
            target: "a:1".into()
        }
        .class(),
        Interactive,
        "UDP 多是实时流量（DNS、游戏、QUIC），和 SvcDatagram 一样"
    );
}

#[test]
fn classes_are_ordered_by_priority() {
    assert!(TrafficClass::Interactive.priority() > TrafficClass::Normal.priority());
    assert!(TrafficClass::Normal.priority() > TrafficClass::Bulk.priority());
}
