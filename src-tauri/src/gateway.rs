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
    Event { event: Box<AgentEvent> },
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
    /// 续播还没补完（队列到顶先停下，等 credit 腾出空间再接着从 `last_id` 读）。
    replay_pending: bool,
}

impl SessionState {
    fn new(credits: u64, after: Option<String>) -> Self {
        Self { credits, queue: VecDeque::new(), last_id: after, replay_pending: false }
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
                            if state.replay_pending {
                                // journal-first：这条 live 早已落进事件日志，继续 replay 会按
                                // event_id 顺序送到。此时入队会跳过未补的历史（last_id 前移），
                                // 所以只让 replay 接管，不入队。
                            } else if state.enqueue(ev) {
                                let _ = send_frame(&mut sink, &ServerFrame::error("overflow", "出站队列溢出，请带 after_event_id 重连")).await;
                                break;
                            }
                        }
                        if pump(&hub, &mut sink, &mut sessions).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        // 广播 lag 丢了帧：从日志按各会话 last_id 补齐（续播同源）。
                        for (session_id, state) in sessions.iter_mut() {
                            state.replay_pending = replay_into(&hub.journal_dir, session_id, state, MAX_QUEUE);
                        }
                        if pump(&hub, &mut sink, &mut sessions).await.is_err() {
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
            let explicit_cursor = after_event_id.is_some();
            let mut state = SessionState::new(credits, after_event_id.clone());
            if explicit_cursor {
                // 带游标 = 断线/新会话续播：从游标之后 replay（分页、受队列上限约束）。
                state.replay_pending = replay_into(&hub.journal_dir, &session_id, &mut state, MAX_QUEUE);
            } else {
                // 省略游标 = 只接 live（docs/04 §1.4）：用日志尾作为去重基线，绝不重放历史。
                // 不种这个游标的话，`last_id = None` 会让 lag 路径/后续续播把整段历史再送一遍，
                // 客户端看到的是重复内容与错乱的助手流 revision（真机 B-20 复发的直接原因）。
                match event_journal::latest_event_id(&hub.journal_dir, &session_id) {
                    Ok(last) => state.last_id = last,
                    Err(e) => tracing::warn!("gateway 读取日志尾失败（{session_id}）: {e}"),
                }
            }
            sessions.insert(session_id, state);
            // 初始 credit 为 0 时队列先攒着（replay_pending 保留），等 `credit` 帧再推进。
            pump(hub, sink, sessions).await
        }
        ClientFrame::Unsubscribe { session_id } => {
            sessions.remove(&session_id);
            Ok(())
        }
        ClientFrame::Credit { session_id, credits } => {
            if let Some(state) = sessions.get_mut(&session_id) {
                state.credits = state.credits.saturating_add(credits);
            }
            pump(hub, sink, sessions).await
        }
    }
}

/// 按 credit 把各会话队列发出去，并在腾出空间后继续未补完的日志续播。
///
/// 续播**不能**一次把整段历史塞进队列：长会话的事件数远超 `MAX_QUEUE`。这里每轮先按
/// credit 发队列，再给还有空间的会话继续读日志，直到没有可推进的会话为止。读失败只
/// 告警（续播是尽力而为的恢复面），不把 `replay_pending` 卡死成永久静默。
async fn pump(
    hub: &GatewayHub,
    sink: &mut WsSink,
    sessions: &mut HashMap<String, SessionState>,
) -> Result<(), String> {
    loop {
        for state in sessions.values_mut() {
            while state.credits > 0 {
                let Some(event) = state.queue.pop_front() else { break };
                send_frame(sink, &ServerFrame::Event { event: Box::new(event) }).await?;
                state.credits -= 1;
            }
        }
        let mut replayed = false;
        for (session_id, state) in sessions.iter_mut() {
            if state.replay_pending && state.queue.len() < MAX_QUEUE {
                state.replay_pending = replay_into(&hub.journal_dir, session_id, state, MAX_QUEUE);
                replayed = true;
            }
        }
        if !replayed {
            break;
        }
    }
    Ok(())
}

/// 从事件日志把 `last_id` 之后的信封分页入队（去重由 `SessionState.enqueue` 保证，
/// 队列上限由 `max_queue` 控制）。返回是否还有没补完的历史（true = 队列到顶停下，
/// 等 credit 腾空间后从新的 `last_id` 续补）。
fn replay_into(
    journal_dir: &Path,
    session_id: &str,
    state: &mut SessionState,
    max_queue: usize,
) -> bool {
    let mut cursor = state.last_id.clone();
    loop {
        let page = match event_journal::read_after(journal_dir, session_id, cursor.as_deref(), REPLAY_LIMIT) {
            Ok(page) => page,
            Err(e) => {
                tracing::warn!("gateway 续播读取失败（{session_id}）: {e}");
                return false;
            }
        };
        for event in page.events {
            cursor = Some(event.event_id.clone());
            // `enqueue` 的溢出判定基于生产上限；这里用显式上限给单测留出可验证的小队列。
            if state.enqueue(event) || state.queue.len() > max_queue {
                return true;
            }
        }
        if !page.has_more {
            return false;
        }
    }
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

    fn temp_dir() -> crate::test_support::TestTempDir {
        crate::test_support::TestTempDir::new("gw")
    }

    /// 起一个真实 loopback server，返回 (hub, url, journal_dir)。
    async fn spawn_server() -> (GatewayHub, String, crate::test_support::TestTempDir) {
        let dir = temp_dir();
        let hub = GatewayHub::new(dir.to_path_buf());
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
        let (_hub, url, _dir) = spawn_server().await;
        let (mut ws, _) = connect_async(&url).await.unwrap();
        ws.send(Message::text(r#"{"type":"hello","token":"bogus"}"#)).await.unwrap();
        let msg = ws.next().await.unwrap().unwrap();
        let Message::Text(t) = msg else { panic!("期望 error 帧") };
        assert!(t.as_str().contains("unauthorized"));
        // 随后连接被关闭
        let closed = ws.next().await;
        assert!(closed.is_none() || matches!(closed, Some(Ok(Message::Close(_)))));
    }

    #[tokio::test]
    async fn live_only_subscription_never_replays_and_routes_by_session() {
        let (hub, url, dir) = spawn_server().await;
        // 种一条历史：省略 after_event_id 的 live-only 订阅绝不能收到它。
        event_journal::append(&dir, &event("s1", "01")).unwrap();
        let mut ws = hello_ok(&url, &hub).await;
        ws.send(Message::text(r#"{"type":"subscribe","session_id":"s1","credits":1}"#)).await.unwrap();
        // 给服务端处理 subscribe 的时间窗口：这期间不应有 replay 帧。
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(timeout(Duration::from_millis(100), ws.next()).await.is_err(), "live-only 不得重放历史");

        // s2 未订阅必须丢弃；s1 的 live 事件按 credit 放行。
        hub.publish(&event("s2", "09"));
        hub.publish(&event("s1", "02"));
        let first = next_event(&mut ws).await;
        assert_eq!(first.session_id, "s1");
        assert_eq!(first.event_id, "02");

        // credit 耗尽：s1(03) 排队等待，不能越过 credit 闸门。
        hub.publish(&event("s1", "03"));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(timeout(Duration::from_millis(100), ws.next()).await.is_err());
        ws.send(Message::text(r#"{"type":"credit","session_id":"s1","credits":1}"#)).await.unwrap();
        let second = next_event(&mut ws).await;
        assert_eq!(second.event_id, "03");
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
    }

    /// 续播超过队列上限时必须停下、等 credit 腾空间后从 `last_id` 继续，不能留下缺口。
    #[test]
    fn replay_resumes_after_queue_cap_without_gaps() {
        let dir = temp_dir();
        for index in 0..10 {
            event_journal::append(&dir, &event("s1", &format!("{index:02}"))).unwrap();
        }
        let mut state = SessionState::new(0, Some(String::new()));
        let mut seen: Vec<String> = Vec::new();
        loop {
            let pending = replay_into(&dir, "s1", &mut state, 3);
            while let Some(ev) = state.queue.pop_front() {
                seen.push(ev.event_id);
            }
            if !pending {
                break;
            }
        }
        let expected: Vec<String> = (0..10).map(|index| format!("{index:02}")).collect();
        assert_eq!(seen, expected);
    }
}
