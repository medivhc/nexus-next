//! 控制入口：命令行和常驻程序之间的本地连接。
//!
//! 两种平台都表现为「一条连接就是一个会话」，帧协议完全共用：
//!
//! - Unix：域套接字文件。本人入口 `0600`，agent 入口 `0666`（任何本机账号都能连，凭据才是授权）。
//! - Windows：没有域套接字可用。本人入口用**命名管道**（默认 DACL 只给创建者，等价于 `0600`）；
//!   agent 入口用**回环 TCP**（连接本身不授权，协议里的凭据才是，等价于 `0666` + 凭据）。
//!
//! Windows 上端点地址写进 `path` 指向的文件（管道名或 `127.0.0.1:端口`），
//! 这样调用方拿到的路径语义和 Unix 完全一致，权限也仍由文件系统把关。

use std::io;
use std::path::Path;
use tokio::io::{AsyncRead, AsyncWrite};

/// 已连接的控制流。两个平台都能读能写，擦除成 trait object 让帧代码只写一份。
pub trait CtlStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> CtlStream for T {}
pub type BoxedCtlStream = Box<dyn CtlStream>;

/// 这个入口允许谁连。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CtlKind {
    /// 本人专用：只有数据目录的主人能连（Unix 的 `0600`）。
    Owner,
    /// agent 入口：本机任何账号都能连，但必须出示 agent 凭据（Unix 的 `0666`）。
    Agent,
}

/// 监听中的控制入口。
pub enum CtlListener {
    #[cfg(unix)]
    Unix(tokio::net::UnixListener),
    #[cfg(windows)]
    Pipe(PipeListener),
    #[cfg(windows)]
    Tcp(tokio::net::TcpListener),
}

#[cfg(windows)]
struct PipeListener {
    name: String,
    server: tokio::net::windows::named_pipe::NamedPipeServer,
}

/// 在 `path` 上开一个控制入口。
///
/// Unix 上 `path` 就是套接字文件；Windows 上它是端点信息文件。
pub async fn bind(path: &Path, kind: CtlKind) -> io::Result<CtlListener> {
    #[cfg(unix)]
    {
        let _ = std::fs::remove_file(path);
        let listener = tokio::net::UnixListener::bind(path)?;
        // 本人入口只给自己；agent 入口给本机所有账号（它们仍需凭据）。
        let mode = match kind {
            CtlKind::Owner => 0o600,
            CtlKind::Agent => 0o666,
        };
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
        Ok(CtlListener::Unix(listener))
    }

    #[cfg(windows)]
    {
        match kind {
            CtlKind::Owner => {
                let name = pipe_name(path);
                let server = win::create_pipe(&name)?;
                write_endpoint(path, &name)?;
                Ok(CtlListener::Pipe(PipeListener { name, server }))
            }
            // agent 入口：连接不是授权，凭据才是，所以回环 TCP 就够。
            CtlKind::Agent => {
                let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
                let addr = listener.local_addr()?;
                write_endpoint(path, &addr.to_string())?;
                Ok(CtlListener::Tcp(listener))
            }
        }
    }
}

/// 连上一个已经 `bind` 好的控制入口。
pub async fn connect(path: &Path) -> io::Result<BoxedCtlStream> {
    #[cfg(unix)]
    {
        Ok(Box::new(tokio::net::UnixStream::connect(path).await?))
    }

    #[cfg(windows)]
    {
        let target = std::fs::read_to_string(path)?.trim().to_string();
        if target.starts_with(r"\\.\pipe\") || target.starts_with("\\??\\") {
            use tokio::net::windows::named_pipe::ClientOptions;
            // `open` 就是 CreateFile：等到有实例可用并连上；模式默认字节流，与创建端一致。
            let client = ClientOptions::new().open(&target)?;
            Ok(Box::new(client))
        } else {
            let s = tokio::net::TcpStream::connect(target).await?;
            let _ = s.set_nodelay(true);
            Ok(Box::new(s))
        }
    }
}

impl CtlListener {
    /// 等下一个客户端。出错时由调用方决定是否继续。
    pub async fn accept(&mut self) -> io::Result<BoxedCtlStream> {
        match self {
            #[cfg(unix)]
            CtlListener::Unix(l) => {
                let (stream, _) = l.accept().await?;
                Ok(Box::new(stream))
            }
            #[cfg(windows)]
            CtlListener::Tcp(l) => {
                let (stream, _) = l.accept().await?;
                let _ = stream.set_nodelay(true);
                Ok(Box::new(stream))
            }
            #[cfg(windows)]
            CtlListener::Pipe(p) => {
                // 命名管道每服务一个连接就要重建一个实例。
                p.server.connect().await?;
                let connected = std::mem::replace(&mut p.server, win::recreate_pipe(&p.name)?);
                Ok(Box::new(connected))
            }
        }
    }
}

/// Windows 下端点信息文件的内容：管道名或 `127.0.0.1:端口`。
#[cfg(windows)]
fn write_endpoint(path: &Path, target: &str) -> io::Result<()> {
    // 先写同目录的临时文件再改名：读的人看到的要么是旧端点，要么是新端点。
    let tmp = path.with_extension("endpoint.tmp");
    std::fs::write(&tmp, target.as_bytes())?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// 由端点路径推出稳定的管道名。同一个数据目录永远得到同一个名字。
#[cfg(windows)]
fn pipe_name(path: &Path) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    path.to_string_lossy().to_lowercase().hash(&mut h);
    format!(r"\\.\pipe\nexus-ctl-{:016x}", h.finish())
}

/// 命名管道的创建与重建。只用到 tokio 的安全接口，不引入 unsafe。
#[cfg(windows)]
mod win {
    use super::io;
    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};

    const MAX_INSTANCES: usize = 64;

    /// 第一个实例：独占创建并拒绝远程 SMB 客户端，禁止回退附着到未知进程已占用的同名管道。
    pub fn create_pipe(name: &str) -> io::Result<NamedPipeServer> {
        let mut opts = ServerOptions::new();
        opts.first_pipe_instance(true)
            .reject_remote_clients(true)
            .max_instances(MAX_INSTANCES);
        opts.create(name)
    }

    /// 服务完一个连接后为下一个客户端重建实例，同样拒绝远程客户端。
    pub fn recreate_pipe(name: &str) -> io::Result<NamedPipeServer> {
        let mut opts = ServerOptions::new();
        opts.reject_remote_clients(true)
            .max_instances(MAX_INSTANCES)
            .create(name)
    }
}
