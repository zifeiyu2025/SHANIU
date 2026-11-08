//! 仅测试：最小 HTTP/1.1 靶子服务器。
//!
//! 为什么自建而不是引第三方 mock 库：这里只需要「回一条固定响应 / 记一次命中」，
//! 用 tokio 的 TcpListener 不到百行，避免为测试再引入一个依赖。
//!
//! 目标地址仍是 127.0.0.1，与生产完全隔离；`WebClient` 的私网拦截照常生效，
//! 测试通过 `with_test_dial` 把「校验用的公网域名」映射到本地端口。

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

type Handler = Arc<dyn Fn(&str) -> String + Send + Sync>;

pub struct TestServer {
    pub addr: SocketAddr,
    hits: Arc<AtomicUsize>,
    handle: tokio::task::JoinHandle<()>,
}

impl TestServer {
    /// 启动监听；`handler` 接收原始请求头文本，返回完整响应字节。
    pub async fn start(handler: impl Fn(&str) -> String + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("测试服务器绑定失败");
        let addr = listener.local_addr().expect("取本地端口失败");
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        let handler: Handler = Arc::new(handler);
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let counter = counter.clone();
                let handler = handler.clone();
                tokio::spawn(async move {
                    let mut buffer = vec![0u8; 16 * 1024];
                    let Ok(read) = socket.read(&mut buffer).await else {
                        return;
                    };
                    if read == 0 {
                        return;
                    }
                    counter.fetch_add(1, Ordering::SeqCst);
                    let request = String::from_utf8_lossy(&buffer[..read]).into_owned();
                    let response = handler(&request);
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        Self { addr, hits, handle }
    }

    /// 已经收到的请求数；用于断言「被拒绝的跳转没有真的发出去」。
    pub fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// 拼一条带 `Content-Length` 的响应。`extra_headers` 形如 `"Location: ...\r\n"`。
pub fn response(status: &str, content_type: &str, extra_headers: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\n{extra_headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}
