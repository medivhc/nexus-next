//! 公共碰头服务（https://dns.iroh.link/pkarr）上的往返：要连外网，默认不跑。
//! CI 在 GitHub 的 runner 上用 `cargo test -p nexus-fabric-iroh --test public_rendezvous -- --ignored` 跑一次，失败不挡提交。

use nexus_fabric::Transport;
use nexus_fabric_iroh::{Config, IrohTransport};

#[tokio::test(flavor = "multi_thread")]
#[ignore = "要连外网"]
async fn put_get_and_clear_on_the_public_service() {
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).unwrap();
    let t = IrohTransport::bind(seed, Config::default()).await.unwrap();
    let mut key = [0u8; 32];
    getrandom::fill(&mut key).unwrap();
    let data: Vec<u8> = (0..150u8).collect();
    t.rendezvous_put(key, data.clone()).await.expect("发布");
    assert_eq!(t.rendezvous_get(key).await.expect("取回"), Some(data));
    // 新的时间戳要比旧的大（微秒）。
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    t.rendezvous_put(key, Vec::new()).await.expect("清掉");
    assert_eq!(t.rendezvous_get(key).await.expect("再取"), None);
    let mut never = [0u8; 32];
    getrandom::fill(&mut never).unwrap();
    assert_eq!(t.rendezvous_get(never).await.expect("没有的位置"), None);
}
