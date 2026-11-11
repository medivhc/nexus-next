//! 驱动：`System` 的仿真实现。时间来自 turmoil 的模拟时钟，随机数来自种子，
//! 存储在内存里并且可以跨「重启」保留。
#![forbid(unsafe_code)]

use nexus_core_proto::Timestamp;
use nexus_sys::System;
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::io;
use std::rc::Rc;

/// 仿真世界的起始时间：2026-01-01T00:00:00Z。
pub const SIM_EPOCH_MS: u64 = 1_767_225_600_000;

/// 一个节点的「磁盘」。克隆后共享同一份内容，用来模拟重启后数据仍在。
#[derive(Clone, Default)]
pub struct SimDisk(Rc<RefCell<BTreeMap<String, Vec<u8>>>>);

/// 仿真世界共用的时钟偏移。拨动它相当于所有机器的时钟一起向前跳了一段，
/// 用来验证「过期」这类只取决于当前时间的行为，而不必真的模拟这段时间里的每一毫秒。
#[derive(Clone, Default)]
pub struct SimClock(Rc<Cell<u64>>);

impl SimClock {
    pub fn jump(&self, d: std::time::Duration) {
        self.0.set(self.0.get() + d.as_millis() as u64);
    }
}

pub struct SimSystem {
    disk: SimDisk,
    rng: Cell<u64>,
    clock: SimClock,
}

impl SimSystem {
    pub fn new(disk: SimDisk, seed: u64, clock: SimClock) -> Self {
        Self { disk, rng: Cell::new(seed ^ 0x9e37_79b9_7f4a_7c15), clock }
    }

    fn next_u64(&self) -> u64 {
        // splitmix64：足够用于可复现的测试数据，不用于真实密钥。
        let mut z = self.rng.get().wrapping_add(0x9e37_79b9_7f4a_7c15);
        self.rng.set(z);
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
}

impl System for SimSystem {
    fn now(&self) -> Timestamp {
        let elapsed = turmoil::sim_elapsed().unwrap_or_default();
        Timestamp(SIM_EPOCH_MS + self.clock.0.get() + elapsed.as_millis() as u64)
    }

    fn random(&self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            let v = self.next_u64().to_le_bytes();
            chunk.copy_from_slice(&v[..chunk.len()]);
        }
    }

    fn store_get(&self, key: &str) -> io::Result<Option<Vec<u8>>> {
        Ok(self.disk.0.borrow().get(key).cloned())
    }

    fn store_put(&self, key: &str, value: &[u8]) -> io::Result<()> {
        self.disk.0.borrow_mut().insert(key.to_string(), value.to_vec());
        Ok(())
    }
}
