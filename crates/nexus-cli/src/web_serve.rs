//! `nexus web serve`：在本机托管网页节点的页面。只提供目录里的静态文件，只监听本机地址。

use nexus_cli::{Sink, EXIT_INTERNAL, EXIT_INVALID_INPUT};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub async fn run<K: Sink>(sink: &mut K, dir: &Path, listen: &str) -> i32 {
    let fail = |sink: &mut K, code: i32, kind: &str, message: String| {
        sink.err(format!("{}\n", serde_json::json!({ "error": kind, "message": message })).as_bytes());
        code
    };
    let addr: SocketAddr = match listen.parse() {
        Ok(a) => a,
        Err(_) => return fail(sink, EXIT_INVALID_INPUT, "invalid_input", format!("--listen 应当是 地址:端口，例如 127.0.0.1:8080：{listen}")),
    };
    if !addr.ip().is_loopback() {
        return fail(sink, EXIT_INVALID_INPUT, "invalid_input", format!("只能监听本机地址（127.0.0.1 或 ::1）：{listen}"));
    }
    let root = match std::fs::canonicalize(dir) {
        Ok(r) if r.join("index.html").is_file() => r,
        _ => return fail(sink, EXIT_INVALID_INPUT, "invalid_input", format!("{} 里没有 index.html", dir.display())),
    };
    let listener = match TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => return fail(sink, EXIT_INTERNAL, "internal", format!("无法监听 {listen}：{e}")),
    };
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(addr.port());
    sink.out(format!("页面：http://localhost:{port}（按 Ctrl-C 停止）\n").as_bytes());
    loop {
        let Ok((stream, _)) = listener.accept().await else { continue };
        let root = root.clone();
        tokio::spawn(async move {
            let _ = serve(stream, &root).await;
        });
    }
}

async fn serve(mut stream: TcpStream, root: &Path) -> std::io::Result<()> {
    let head = read_head(&mut stream).await?;
    respond(&mut stream, root, &head).await
}

/// 读到请求头结束（或 8 KiB）为止。返回读到的全部字节，可能包含请求体的开头。
pub async fn read_head<R: AsyncRead + Unpin>(stream: &mut R) -> std::io::Result<Vec<u8>> {
    let mut head = Vec::new();
    let mut buf = [0u8; 1024];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") && head.len() < 8192 {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        head.extend_from_slice(&buf[..n]);
    }
    Ok(head)
}

/// 请求行里的方法和目标。
pub fn request_line(head: &[u8]) -> (String, String) {
    let line = String::from_utf8_lossy(head);
    let mut parts = line.split_whitespace();
    (parts.next().unwrap_or("").to_string(), parts.next().unwrap_or("").to_string())
}

/// 按请求头回应目录里的静态文件，然后关闭。
pub async fn respond<W: AsyncWrite + Unpin>(stream: &mut W, root: &Path, head: &[u8]) -> std::io::Result<()> {
    let (method, target) = request_line(head);
    let (method, target) = (method.as_str(), target.as_str());
    let found = match method {
        "GET" | "HEAD" => resolve(root, target).and_then(|p| std::fs::read(&p).ok().map(|b| (p, b))),
        _ => None,
    };
    let (status, ctype, body) = match found {
        Some((path, body)) => ("200 OK", content_type(&path), body),
        None => ("404 Not Found", "text/plain; charset=utf-8", b"not found\n".to_vec()),
    };
    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-cache\r\n\
         X-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).await?;
    if method != "HEAD" {
        stream.write_all(&body).await?;
    }
    stream.shutdown().await
}

/// 请求的路径对应的文件；不在目录里（含 `..`、符号链接指向外面）或不存在时为 `None`。
pub fn resolve(root: &Path, target: &str) -> Option<PathBuf> {
    let path = target.split(['?', '#']).next()?;
    let decoded = percent_decode(path)?;
    let mut out = root.to_path_buf();
    for seg in decoded.split('/') {
        match seg {
            "" | "." => {}
            ".." => return None,
            s if s.contains('\\') || s.contains('\0') => return None,
            s => out.push(s),
        }
    }
    if out == root || decoded.ends_with('/') {
        out.push("index.html");
    }
    let real = std::fs::canonicalize(&out).ok()?;
    (real.starts_with(root) && real.is_file()).then_some(real)
}

fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "wasm" => "application/wasm",
        "css" => "text/css; charset=utf-8",
        "json" | "webmanifest" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_stay_inside_the_page_directory() {
        let tmp = std::env::temp_dir().join(format!("nexus-web-serve-{}", std::process::id()));
        let root = tmp.join("page");
        std::fs::create_dir_all(root.join("pkg")).unwrap();
        std::fs::write(root.join("index.html"), "i").unwrap();
        std::fs::write(root.join("pkg/a b.js"), "a").unwrap();
        std::fs::write(tmp.join("secret"), "s").unwrap();
        let _ = std::os::unix::fs::symlink(tmp.join("secret"), root.join("link"));
        let root = std::fs::canonicalize(&root).unwrap();

        assert_eq!(resolve(&root, "/"), Some(root.join("index.html")));
        assert_eq!(resolve(&root, "/?x=1"), Some(root.join("index.html")));
        assert_eq!(resolve(&root, "/pkg/a%20b.js"), Some(root.join("pkg/a b.js")));
        for bad in ["/../secret", "/pkg/%2e%2e/%2e%2e/secret", "/link", "/pkg", "/nope", "/%zz", "/pkg/..%5c..%5csecret"] {
            assert_eq!(resolve(&root, bad), None, "{bad}");
        }
        assert_eq!(content_type(Path::new("x.wasm")), "application/wasm");
        assert_eq!(content_type(Path::new("x.html")), "text/html; charset=utf-8");
    }
}
