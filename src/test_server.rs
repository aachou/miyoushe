use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// 测试用的最小 HTTP/1.1 响应。
pub struct Resp {
    pub status: u16,
    pub body: Vec<u8>,
    pub content_type: &'static str,
}

impl Resp {
    pub fn json(body: impl Into<String>) -> Self {
        Self {
            status: 200,
            body: body.into().into_bytes(),
            content_type: "application/json",
        }
    }

    pub fn bytes(body: impl Into<Vec<u8>>) -> Self {
        Self {
            status: 200,
            body: body.into(),
            content_type: "image/jpeg",
        }
    }

    pub fn status(status: u16) -> Self {
        Self {
            status,
            body: b"error".to_vec(),
            content_type: "text/plain",
        }
    }
}

pub type Handler = Arc<dyn Fn(&str) -> Resp + Send + Sync>;

/// 启动本地 HTTP 服务，handler 接收「路径+查询串」并返回响应。
pub async fn spawn(handler: Handler) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("绑定端口失败");
    let addr = listener.local_addr().expect("读取地址失败");
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let handler = handler.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 1024];
                loop {
                    match socket.read(&mut tmp).await {
                        Ok(0) => break,
                        Ok(n) => {
                            buf.extend_from_slice(&tmp[..n]);
                            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                                break;
                            }
                        }
                        Err(_) => return,
                    }
                }
                let request = String::from_utf8_lossy(&buf).into_owned();
                let path = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("/")
                    .to_string();
                let resp = handler(&path);
                let reason = match resp.status {
                    200 => "OK",
                    404 => "Not Found",
                    500 => "Internal Server Error",
                    _ => "Unknown",
                };
                let head = format!(
                    "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nContent-Type: {}\r\nConnection: close\r\n\r\n",
                    resp.status,
                    reason,
                    resp.body.len(),
                    resp.content_type
                );
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(&resp.body).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    addr
}

pub fn base_url(addr: SocketAddr) -> String {
    format!("http://{addr}")
}
