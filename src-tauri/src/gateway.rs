//! loopback WebSocket gateway（SH-094 批 2，ADR-005「内部传输路线」）。
//!
//! 定位：只替换 **Rust → 适配层** 的事件传输（取代无背压的 Tauri 全局广播），投递的仍是
//! `AgentEvent` 信封；适配层 → DSH 客户端继续走已实现的 DSH 原生 `session.follow/page`。
//! 因此 credit 背压与 `event_id` 续播是 SHANIU 内部传输特性（上游 DSH 无对应物，2026-11-07
//! 用户拍板保留），不是对上游 remote.mux 协议的照搬。
//!
//! 线契约（JSON text 帧，字段 snake_case）：
//! - C→S：`hello{token}` 鉴权 → `subscribe{session_id, after_event_id?, credits}` /
//!   `unsubscribe{session_id}` / `credit{session_id, credits}`。
//! - S→C：`ready` 握手回执 / `event{event: AgentEvent}` / `error{code, message}`。
//! - 一条连接按 `session_id` 多路复用；每会话独立 credit 窗口与出站队列。
//! - 续播：`subscribe.after_event_id` 省略 = 只接 live；带上则先从事件日志（批 1
//!   `event_journal`）补该游标之后的信封，再接 live。`event_id` 单调（ULID），入队按
//!   `last_id` 去重，故 replay 与 live 交叠不重发。
//! - 背压：每会话 credit 计数，发一条减一；耗尽则排队等 `credit` 帧。队列超 `MAX_QUEUE`
//!   fail-closed 关连接（客户端带 `after_event_id` 重连补帧）。broadcast  lag 同样从日志补齐。
//!
//! 鉴权：仅监听 127.0.0.1；token 为进程级随机串（两个 ULID = 160 bit CSPRNG 熵），经控制面
//! `gateway_info` 命令注入前端，首帧 `hello` 校验，错则 `error{unauthorized}` + 关闭。

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use crate::event_journal;
use crate::events::AgentEvent;

/// 广播通道容量：覆盖工具输出 burst；消费端 lag 时从日志补齐，不靠无限缓冲。
const BROADCAST_CAPACITY: usize = 1024;
/// 每会话出站队列硬上限：超过即 fail-closed 关连接（客户端重连续播）。
const MAX_QUEUE: usize = 8192;
/// 首帧 hello 的等待时限：idle 连接不占用资源。
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
/// 续播单页条数（复用事件日志的硬上限）。
const REPLAY_LIMIT: usize = event_journal::MAX_LIMIT;

/// 进程级 gateway 句柄（Clone 廉价）：广播发送端 + token + 绑定端口 + 日志目录。
/// 组合根创建并 manage；sink 持发送端发布事件，server 任务持接收端扇出。
#[derive(Clone)]
pub struct GatewayHub {
    sender: broadcast::Sender<AgentEvent>,
    token: String,
    port: Arc<AtomicU16>,
    journal_dir: PathBuf,
}

impl GatewayHub {
    pub fn new(journal_dir: PathBuf) -> Self {
        let (sender, _) = broadcast::channel(BROADCAST_CAPACITY);
        Self {
            sender,
            token: generate_token(),
            port: Arc::new(AtomicU16::new(0)),
            journal_dir,
        }
    }

    /// sink 发布一条信封。无接收端时 send 返回 Err（值丢弃）——续播由日志兜底，忽略即可。
    pub fn publish(&self, event: &AgentEvent) {
        let _ = self.sender.send(event.clone());
    }

    /// 广播发送端克隆：组合根注入 `TauriEventSink`（events 只持 tokio Sender，不引用本模块）。
    pub fn broadcast_sender(&self) -> broadcast::Sender<AgentEvent> {
        self.sender.clone()
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    /// 绑定端口（0 = 尚未绑定）。`gateway_info` 命令据此回报前端。
    pub fn port(&self) -> u16 {
        self.port.load(Ordering::SeqCst)
    }

    fn receiver(&self) -> broadcast::Receiver<AgentEvent> {
        self.sender.subscribe()
    }
}

/// 两个 ULID 拼接 = 160 bit CSPRNG 随机熵（ulid 底层 rand ChaCha），URL 安全、无需新依赖。
fn generate_token() -> String {
    format!("{}{}", ulid::Ulid::generate(), ulid::Ulid::generate())
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientFrame {
    Hello { token: String },
    Subscribe {
        session_id: String,
        #[serde(default)]
        after_event_id: Option<String>,
        #[serde(default)]
        credits: u64,
    },
    Unsubscribe { session_id: String },
    Credit { session_id: String, credits: u64 },
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerFrame {
    Ready,
    Event { event: AgentEvent },
    Error { code: String, message: String },
}

impl ServerFrame {
    fn error(code: &str, message: impl Into<String>) -> Self {
        Self::Error { code: code.into(), message: message.into() }
    }
}

/// 绑定 loopback 动态端口并跑 accept 循环（随 app 存活；调用方 spawn 到 tokio）。
/// 端口写入 hub 供 `gateway_info` 回报。绑定失败如实返回错误（组合根决定降级）。
pub async fn serve(hub: GatewayHub, listener: TcpListener) -> std::io::Result<()> {
    let port = listener.local_addr()?.port();
    hub.port.store(port, Ordering::SeqCst);
    tracing::info!("WebSocket gateway 监听 127.0.0.1:{port}");
    loop {
        let (stream, _peer) = listener.accept().await?;
        let hub = hub.clone();
        tokio::spawn(async move {
            match tokio_tungstenite::accept_async(stream).await {
                Ok(ws) => handle_connection(hub, ws).await,
                Err(e) => tracing::warn!("WebSocket 握手失败: {e}"),
            }
        });
    }
}

/// 每会话订阅态：credit 窗口 + 出站队列 + 末条入队 event_id（去重/续播游标）。
struct SessionState {
    credits: u64,
    queue: VecDeque<AgentEvent>,
    last_id: Option<String>,
}

impl SessionState {
    fn new(credits: u64, after: Option<String>) -> Self {
        Self { credits, queue: VecDeque::new(), last_id: after }
    }

    /// 入队一条信封：event_id 必须严格大于 last_id（单调去重）。返回是否队列溢出。
    fn enqueue(&mut self, event: AgentEvent) -> bool {
        if let Some(last) = &self.last_id {
            if event.event_id.as_str() <= last.as_str() {
                return false;
            }
        }
        self.last_id = Some(event.event_id.clone());
        self.queue.push_back(event);
        self.queue.len() > MAX_QUEUE
    }
}

type WsSink = futures::stream::SplitSink<WebSocketStream<TcpStream>, Message>;

async fn send_frame(sink: &mut WsSink, frame: &ServerFrame) -> Result<(), String> {
    let json = serde_json::to_string(frame).map_err(|e| format!(" gateway 帧序列化失败: {e}"))?;
    sink.send(Message::text(json)).await.map_err(|e| format!("gateway 发送失败: {e}"))
}

async fn handle_connection(hub: GatewayHub, ws: WebSocketStream<TcpStream>) {
    let (mut sink, mut stream) = ws.split();
    // 鉴权：等首帧 hello。超时/非 hello/token 错 → 关闭。
    let authed = match timeout(HELLO_TIMEOUT, stream.next()).await {
        Ok(Some(Ok(Message::Text(text)))) => {
            match serde_json::from_str::<ClientFrame>(text.as_str()) {
                Ok(ClientFrame::Hello { token }) if token == hub.token() => true,
                Ok(ClientFrame::Hello { .. }) => {
                    let _ = send_frame(&mut sink, &ServerFrame::error("unauthorized", "token 不匹配")).await;
                    false
                }
                _ => {
                    let _ = send_frame(&mut sink, &ServerFrame::error("bad-request", "首帧必须是 hello")).await;
                    false
                }
            }
        }
        _ => false,
    };
    if !authed {
        let _ = sink.close().await;
        return;
    }
    if send_frame(&mut sink, &ServerFrame::Ready).await.is_err() {
        return;
    }

    let mut rx = hub.receiver();
    let mut sessions: HashMap<String, SessionState> = HashMap::new();
    loop {
        tokio::select! {
            incoming = stream.next() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<ClientFrame>(text.as_str()) {
                            Ok(frame) => {
                                if handle_frame(&hub, &mut sink, &mut sessions, frame).await.is_err() {
                                    break;
                                }
                            }
                            Err(e) => {
                                if send_frame(&mut sink, &ServerFrame::error("bad-request", format!("帧解析失败: {e}"))).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(Message::Binary(_))) => {
                        // 契约只走 text；binary 视为协议错（上游同口径 close 1003）。
                        let _ = send_frame(&mut sink, &ServerFrame::error("protocol", "只接受 text 帧")).await;
                        break;
                    }
                    Some(Ok(_)) => {} // Ping/Pong 由 tungstenite 自动应答
                    Some(Err(_)) => break,
                }
            }
            event = rx.recv() => {
                match event {
                    Ok(ev) => {
                        let session_id = ev.session_id.clone();
                        if let Some(state) = sessions.get_mut(&session_id) {
                            if state.enqueue(ev) {
                                let _ = send_frame(&mut sink, &ServerFrame::error("overflow", "出站队列溢出，请带 after_event_id 重连")).await;
                                break;
                            }
                        }
                        if flush_all(&mut sink, &mut sessions).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        // 广播 lag 丢了帧：从日志按各会话 last_id 补齐（续播同源）。
                        for (session_id, state) in sessions.iter_mut() {
                            replay_into(&hub.journal_dir, session_id, state);
                        }
                        if flush_all(&mut sink, &mut sessions).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
    let _ = sink.close().await;
}

async fn handle_frame(
    hub: &GatewayHub,
    sink: &mut WsSink,
    sessions: &mut HashMap<String, SessionState>,
    frame: ClientFrame,
) -> Result<(), String> {
    match frame {
        ClientFrame::Hello { .. } => {
            send_frame(sink, &ServerFrame::error("bad-request", "已鉴权，勿重发 hello")).await
        }
        ClientFrame::Subscribe { session_id, after_event_id, credits } => {
            let mut state = SessionState::new(credits, after_event_id.clone());
            replay_into(&hub.journal_dir, &session_id, &mut state);
            sessions.insert(session_id, state);
            flush_all(sink, sessions).await
        }
        ClientFrame::Unsubscribe { session_id } => {
            sessions.remove(&session_id);
            Ok(())
        }
        ClientFrame::Credit { session_id, credits } => {
            if let Some(state) = sessions.get_mut(&session_id) {
                state.credits = state.credits.saturating_add(credits);
            }
            flush_all(sink, sessions).await
        }
    }
}

/// 从事件日志把 `after_id` 之后的信封分页入队（去重由 SessionState.enqueue 保证）。
/// 读失败只告警：续播是尽力而为的恢复面，不替协议层掩盖缺失（与批 1 同口径）。
fn replay_into(journal_dir: &Path, session_id: &str, state: &mut SessionState) {
    let mut cursor = state.last_id.clone();
    loop {
        let page = match event_journal::read_after(journal_dir, session_id, cursor.as_deref(), REPLAY_LIMIT) {
            Ok(page) => page,
            Err(e) => {
                tracing::warn!("gateway 续播读取失败（{session_id}）: {e}");
                return;
            }
        };
        for event in page.events {
            cursor = Some(event.event_id.clone());
            if state.enqueue(event) {
                return; // 队列满：停补，等 credit  drain 后由 live/lag 路径再补
            }
        }
        if !page.has_more {
            return;
        }
    }
}

/// 按 credit 把各会话队列尽可能发出。发送失败向上传播（连接已断）。
async fn flush_all(sink: &mut WsSink, sessions: &mut HashMap<String, SessionState>) -> Result<(), String> {
    for state in sessions.values_mut() {
        while state.credits > 0 {
            let Some(event) = state.queue.pop_front() else { break };
            send_frame(sink, &ServerFrame::Event { event }).await?;
            state.credits -= 1;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::EventMsg;
    use tokio_tungstenite::connect_async;

    fn event(session: &str, id: &str) -> AgentEvent {
        AgentEvent {
            event_id: id.into(),
            session_id: session.into(),
            agent_id: "root".into(),
            parent_id: None,
            ts_ms: 1,
            msg: EventMsg::TurnStarted { turn: 1 },
        }
    }

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("shaniu-gw-{}-{}", std::process::id(), ulid::Ulid::generate()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 起一个真实 loopback server，返回 (hub, url, journal_dir)。
    async fn spawn_server() -> (GatewayHub, String, PathBuf) {
        let dir = temp_dir();
        let hub = GatewayHub::new(dir.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server_hub = hub.clone();
        tokio::spawn(async move { serve(server_hub, listener).await.unwrap() });
        (hub, format!("ws://127.0.0.1:{port}"), dir)
    }

    async fn hello_ok(url: &str, hub: &GatewayHub) -> WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>> {
        let (mut ws, _) = connect_async(url).await.unwrap();
        ws.send(Message::text(serde_json::to_string(&ClientFrame::Hello { token: hub.token().into() }).unwrap())).await.unwrap();
        let ready = ws.next().await.unwrap().unwrap();
        assert!(matches!(ready, Message::Text(t) if t.as_str().contains("\"ready\"")));
        ws
    }

    async fn next_event(ws: &mut WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>) -> AgentEvent {
        let msg = ws.next().await.unwrap().unwrap();
        let Message::Text(t) = msg else { panic!("期望 event text 帧，实得 {msg:?}") };
        let frame: serde_json::Value = serde_json::from_str(t.as_str()).unwrap();
        assert_eq!(frame["type"], "event");
        serde_json::from_value(frame["event"].clone()).unwrap()
    }

    #[tokio::test]
    async fn wrong_token_is_rejected() {
        let (_hub, url, dir) = spawn_server().await;
        let (mut ws, _) = connect_async(&url).await.unwrap();
        ws.send(Message::text(r#"{"type":"hello","token":"bogus"}"#)).await.unwrap();
        let msg = ws.next().await.unwrap().unwrap();
        let Message::Text(t) = msg else { panic!("期望 error 帧") };
        assert!(t.as_str().contains("unauthorized"));
        // 随后连接被关闭
        let closed = ws.next().await;
        assert!(closed.is_none() || matches!(closed, Some(Ok(Message::Close(_)))));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn subscribe_routes_by_session_and_credit_gates() {
        let (hub, url, dir) = spawn_server().await;
        // 先落盘一条 s1 信封：subscribe 时经 replay 投递，收到它即证明订阅已在服务端生效。
        // 此后的 live 发布不再有 subscribe/publish 竞态——与生产 journal-first 契约同口径
        // （TauriEventSink 先 append 再 broadcast，journal 是续播兜底的事实来源）。
        event_journal::append(&dir, &event("s1", "02")).unwrap();
        let mut ws = hello_ok(&url, &hub).await;
        // 订阅 s1，初始 credit=1 → replay 先发 s1(02)，credit 随即耗尽
        ws.send(Message::text(r#"{"type":"subscribe","session_id":"s1","credits":1}"#)).await.unwrap();
        let first = next_event(&mut ws).await;
        assert_eq!(first.session_id, "s1");
        assert_eq!(first.event_id, "02");
        // 订阅已生效：发布未订阅的 s2（必须被丢弃）与 s1 live（credit=0 应排队等待）
        hub.publish(&event("s2", "01"));
        hub.publish(&event("s1", "03"));
        // 再 grant 1 → 只放行 s1(03)；若 s2 被错误路由会先于 03 到达，下面的断言即失败
        ws.send(Message::text(r#"{"type":"credit","session_id":"s1","credits":1}"#)).await.unwrap();
        let second = next_event(&mut ws).await;
        assert_eq!(second.session_id, "s1");
        assert_eq!(second.event_id, "03");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn reconnect_replays_from_journal_after_cursor() {
        let (hub, url, dir) = spawn_server().await;
        // 先落盘三条（模拟上次连接已收到的日志）
        for id in ["01", "02", "03"] {
            event_journal::append(&dir, &event("s1", id)).unwrap();
        }
        let mut ws = hello_ok(&url, &hub).await;
        // 带 after_event_id=01 续播，credit 充足
        ws.send(Message::text(r#"{"type":"subscribe","session_id":"s1","after_event_id":"01","credits":10}"#)).await.unwrap();
        let a = next_event(&mut ws).await;
        let b = next_event(&mut ws).await;
        assert_eq!(a.event_id, "02");
        assert_eq!(b.event_id, "03");
        // live 事件接续，且与 replay 不重发
        hub.publish(&event("s1", "04"));
        let c = next_event(&mut ws).await;
        assert_eq!(c.event_id, "04");
        std::fs::remove_dir_all(&dir).ok();
    }
}