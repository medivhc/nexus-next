//! 到本机常驻程序的控制通道：数据目录里的控制套接字（命令行和 Android 应用的界面共用）。

use crate::{ctl_endpoint, Ctl};
use nexus_fabric::{write_msg, FrameReader};
use nexus_runtime::CtlClient;
use nexus_wire::{AgentCredential, CtlHello, CtlRequest, CtlResponse, Input};
use std::io;
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

/// 命令行一侧：通过 Unix 套接字连接常驻程序，把套接字两个方向接到会话通道上。
#[derive(Clone)]
pub struct UnixCtl {
    socket: PathBuf,
    agent: Option<AgentCredential>,
}

impl UnixCtl {
    /// 用户本人用数据目录里的控制入口；带凭据时用 agent 入口（`NEXUS_AGENT_SOCKET` 或数据目录里的 agent.sock）。
    pub fn new(data_dir: &Path, agent: Option<AgentCredential>) -> Self {
        let socket = match &agent {
            None => data_dir.join("ctl.sock"),
            Some(_) => std::env::var_os("NEXUS_AGENT_SOCKET").map(PathBuf::from).unwrap_or_else(|| data_dir.join("agent.sock")),
        };
        Self { socket, agent }
    }
}

impl Ctl for UnixCtl {
    async fn open(&self, req: CtlRequest) -> io::Result<CtlClient> {
        let mut stream = ctl_endpoint::connect(&self.socket).await?;
        write_msg(&mut stream, &CtlHello { agent: self.agent.clone(), request: req }).await?;
        let (rd, mut wr) = tokio::io::split(stream);
        let (in_tx, mut in_rx) = mpsc::channel::<Input>(16);
        let (resp_tx, resp_rx) = mpsc::channel::<CtlResponse>(16);
        tokio::spawn(async move {
            while let Some(input) = in_rx.recv().await {
                if write_msg(&mut wr, &input).await.is_err() {
                    break;
                }
            }
            let _ = wr.shutdown().await;
        });
        tokio::spawn(async move {
            let mut reader = FrameReader::new(rd);
            while let Ok(Some(resp)) = reader.next::<CtlResponse>().await {
                if resp_tx.send(resp).await.is_err() {
                    break;
                }
            }
        });
        Ok(CtlClient { tx: in_tx, rx: resp_rx })
    }
}

