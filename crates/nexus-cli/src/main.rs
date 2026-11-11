use nexus_cli::{parse, run_client, Cli, Cmd, Ctl, Output, EXIT_INTERNAL};
use nexus_fabric::{read_msg, write_msg, OfflineTransport};
use nexus_runtime::{load_or_create_node_id, CtlCall, Node};
use nexus_sys_real::RealSystem;
use nexus_wire::{CtlRequest, CtlResponse};
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};

fn main() {
    let cli = match parse(std::env::args_os().skip(1)) {
        Ok(cli) => cli,
        Err(out) => finish(out),
    };
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("无法启动运行时");
    let data_dir = data_dir(&cli);
    let out = rt.block_on(async {
        match &cli.cmd {
            Cmd::Daemon { name } => match daemon(&data_dir, name.clone()).await {
                Ok(()) => Output { code: 0, stdout: String::new(), stderr: String::new() },
                Err(e) => Output { code: EXIT_INTERNAL, stdout: String::new(), stderr: format!("nexus daemon: {e}\n") },
            },
            cmd => run_client(cmd, &UnixCtl { socket: data_dir.join("ctl.sock") }).await,
        }
    });
    finish(out)
}

fn finish(out: Output) -> ! {
    print!("{}", out.stdout);
    eprint!("{}", out.stderr);
    std::process::exit(out.code)
}

fn data_dir(cli: &Cli) -> PathBuf {
    if let Some(d) = &cli.data_dir {
        return d.clone();
    }
    if let Some(d) = std::env::var_os("NEXUS_DATA_DIR") {
        return d.into();
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| ".".into());
    home.join(".local/share/nexus")
}

struct UnixCtl {
    socket: PathBuf,
}

impl Ctl for UnixCtl {
    async fn call(&self, req: CtlRequest) -> io::Result<CtlResponse> {
        let mut stream = UnixStream::connect(&self.socket).await?;
        write_msg(&mut stream, &req).await?;
        read_msg(&mut stream).await
    }
}

async fn daemon(data_dir: &Path, name: Option<String>) -> io::Result<()> {
    let system = RealSystem::new(data_dir)?;
    let node_id = load_or_create_node_id(&system)?;
    let name = name
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok().map(|s| s.trim().to_string()))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "node".to_string());
    let node = Node::new(OfflineTransport { node_id }, system, &name);

    let socket = data_dir.join("ctl.sock");
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket)?;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;

    let (tx, mut rx) = mpsc::channel::<CtlCall>(64);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(serve_ctl(stream, tx.clone()));
        }
    });

    let result = tokio::select! {
        r = node.run(&mut rx) => r,
        _ = tokio::signal::ctrl_c() => Ok(()),
        _ = terminate() => Ok(()),
    };
    let _ = std::fs::remove_file(&socket);
    result
}

async fn terminate() {
    use tokio::signal::unix::{signal, SignalKind};
    match signal(SignalKind::terminate()) {
        Ok(mut s) => { s.recv().await; }
        Err(_) => std::future::pending().await,
    }
}

async fn serve_ctl(mut stream: UnixStream, tx: mpsc::Sender<CtlCall>) {
    let Ok(req) = read_msg::<_, CtlRequest>(&mut stream).await else { return };
    let (reply_tx, reply_rx) = oneshot::channel();
    if tx.send((req, reply_tx)).await.is_err() {
        return;
    }
    if let Ok(resp) = reply_rx.await {
        let _ = write_msg(&mut stream, &resp).await;
    }
}
