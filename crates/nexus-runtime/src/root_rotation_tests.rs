use super::*;
use std::collections::BTreeMap;
use tokio::io::DuplexStream;

#[derive(Clone, Default)]
struct MemSystem {
    store: std::rc::Rc<RefCell<BTreeMap<String, Vec<u8>>>>,
    fail_put_key: Option<&'static str>,
}

struct DummyTransport;
struct DummyConn;
struct DummyProc;
struct DummyPty;

impl Transport for DummyTransport {
    type Conn = DummyConn;
    fn node_id(&self) -> NodeId {
        NodeId([1; 32])
    }
    fn local_hint(&self) -> String {
        String::new()
    }
    fn add_hint(&self, _: NodeId, _: &str) {}
    async fn accept(&self) -> io::Result<(DummyConn, nexus_fabric::Proto)> {
        std::future::pending().await
    }
    async fn connect(&self, _: NodeId, _: nexus_fabric::Proto) -> io::Result<DummyConn> {
        Err(io::ErrorKind::ConnectionRefused.into())
    }
}

impl nexus_fabric::Connection for DummyConn {
    type Stream = DuplexStream;
    fn remote(&self) -> NodeId {
        NodeId([2; 32])
    }
    fn path_kind(&self) -> Option<nexus_core_proto::PathKind> {
        None
    }
    async fn open_bi(&self) -> io::Result<DuplexStream> {
        unreachable!()
    }
    async fn accept_bi(&self) -> io::Result<Option<DuplexStream>> {
        unreachable!()
    }
}

impl nexus_sys::Proc for DummyProc {
    async fn next_event(&mut self) -> nexus_sys::ProcEvent {
        unreachable!()
    }
    fn write_stdin(&mut self, _: Vec<u8>) {}
    fn close_stdin(&mut self) {}
    fn kill(&mut self) {}
}

impl nexus_sys::Pty for DummyPty {
    async fn next_event(&mut self) -> nexus_sys::PtyEvent {
        unreachable!()
    }
    fn write(&mut self, _: Vec<u8>) {}
    fn resize(&mut self, _: nexus_wire::Size) {}
    fn kill(&mut self) {}
}

impl System for MemSystem {
    type Proc = DummyProc;
    type Pty = DummyPty;
    type Tcp = DuplexStream;
    async fn tcp_connect(&self, _: std::net::SocketAddr) -> io::Result<DuplexStream> {
        Err(io::ErrorKind::ConnectionRefused.into())
    }
    async fn resolve(&self, _: &str) -> io::Result<Vec<std::net::IpAddr>> {
        Err(io::ErrorKind::NotFound.into())
    }
    fn now(&self) -> Timestamp {
        Timestamp(0)
    }
    fn random(&self, buf: &mut [u8]) {
        buf.fill(0x42);
    }
    fn store_get(&self, key: &str) -> io::Result<Option<Vec<u8>>> {
        Ok(self.store.borrow().get(key).cloned())
    }
    fn store_put(&self, key: &str, value: &[u8]) -> io::Result<()> {
        if self.fail_put_key == Some(key) {
            return Err(io::Error::other(format!("injected failure on {key}")));
        }
        self.store
            .borrow_mut()
            .insert(key.to_string(), value.to_vec());
        Ok(())
    }
    fn store_delete(&self, key: &str) -> io::Result<()> {
        self.store.borrow_mut().remove(key);
        Ok(())
    }
    fn cas_get(&self, _: &[u8; 32]) -> io::Result<Option<Vec<u8>>> {
        Ok(None)
    }
    fn cas_put(&self, _: &[u8; 32], _: &[u8]) -> io::Result<()> {
        Ok(())
    }
    fn cas_has(&self, _: &[u8; 32]) -> io::Result<bool> {
        Ok(false)
    }
    fn append_log(&self, _: &str, _: &[u8], _: u64) -> io::Result<()> {
        Ok(())
    }
    fn read_log(&self, _: &str) -> io::Result<Vec<u8>> {
        Ok(Vec::new())
    }
    fn spawn(&self, _: nexus_sys::Cmd) -> io::Result<DummyProc> {
        unreachable!()
    }
    fn spawn_pty(&self, _: &str, _: nexus_wire::Size, _: Timestamp) -> io::Result<DummyPty> {
        unreachable!()
    }
}

fn setup_rotation_fixture(
    fail_put_key: Option<&'static str>,
) -> (
    Node<DummyTransport, MemSystem>,
    MemSystem,
    SignedRootRotation,
    SignedRoster,
    PublicKey,
) {
    let me = NodeId([1; 32]);
    let other = NodeId([2; 32]);
    let old_seed = [0x11; 32];
    let old_root = SecretKey::from_seed(old_seed);
    let new_root = SecretKey::from_seed([0x22; 32]);

    let v1 = Roster {
        root: old_root.public(),
        version: 1,
        devices: vec![
            Device {
                node_id: me,
                name: "old-admin".into(),
                role: DeviceRole::Admin,
                added_at: Timestamp(1),
            },
            Device {
                node_id: other,
                name: "new-admin".into(),
                role: DeviceRole::Member,
                added_at: Timestamp(2),
            },
        ],
        web_keys: Vec::new(),
        anchor: None,
        revoked_agents: Vec::new(),
    }
    .sign(&old_root);

    let sys = MemSystem {
        store: Default::default(),
        fail_put_key: None,
    };
    save(&sys, KEY_ROOT_PUB, &old_root.public()).unwrap();
    save(&sys, KEY_ROOT_KEY, &old_seed).unwrap();
    save(&sys, KEY_ROSTER, &v1).unwrap();

    let sys = MemSystem {
        store: sys.store,
        fail_put_key,
    };
    let node = Node::new(DummyTransport, sys.clone(), "old-admin").unwrap();

    let v2 = Roster {
        root: new_root.public(),
        version: 2,
        devices: vec![
            Device {
                node_id: me,
                name: "old-admin".into(),
                role: DeviceRole::Member,
                added_at: Timestamp(1),
            },
            Device {
                node_id: other,
                name: "new-admin".into(),
                role: DeviceRole::Admin,
                added_at: Timestamp(2),
            },
        ],
        web_keys: Vec::new(),
        anchor: None,
        revoked_agents: Vec::new(),
    }
    .sign(&new_root);
    let rot = RootRotation {
        old_root: old_root.public(),
        new_root: new_root.public(),
        version: 2,
    }
    .sign(&old_root);
    (node, sys, rot, v2, old_root.public())
}

#[test]
fn adopt_root_rotation_clears_root_key_when_no_longer_admin() {
    let (node, sys, rot, v2, _) = setup_rotation_fixture(None);
    assert!(node.trust.borrow().root_key.is_some());
    assert!(sys.store_get(KEY_ROOT_KEY).unwrap().is_some());

    node.adopt_root_rotation(&rot, &v2)
        .expect("rotation should succeed");

    assert!(node.trust.borrow().root_key.is_none());
    assert!(sys.store_get(KEY_ROOT_KEY).unwrap().is_none());
    assert!(node.admin_key().is_err());
    let reloaded = Node::new(DummyTransport, sys, "old-admin").unwrap();
    assert!(reloaded.trust.borrow().root_key.is_none());
}

#[test]
fn adopt_root_rotation_propagates_rotations_save_error() {
    let (node, _sys, rot, v2, old_root_pub) = setup_rotation_fixture(Some(KEY_ROTATIONS));
    let err = node
        .adopt_root_rotation(&rot, &v2)
        .expect_err("KEY_ROTATIONS save failure must propagate");
    assert_eq!(err.code, ErrorCode::Internal);
    assert_eq!(node.trust.borrow().root, Some(old_root_pub));
    assert!(node.trust.borrow().rotations.is_empty());
}
