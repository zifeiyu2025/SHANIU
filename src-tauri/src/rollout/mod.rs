//! rollout JSONL 持久化（docs/04 §3.1，SH-030）。
//!
//! 一行一个 `RolloutItem`，首行必须是 `SessionMeta`。三条刻意的设计选择：
//!
//! 1. **逐行 flush**：进程被杀/断电时，已写行必须可读。攒批写快，但崩溃点之后的历史全丢，
//!    resume 就会拿到一段"看起来完整、实际缺尾"的历史——比没存档更糟。
//! 2. **只容忍最后一行撕裂**：读的时候，最后一行如果不是完整 JSON 就当半截写丢弃；
//!    **中间**任何一行解析失败一律报错。跳过坏行等于悄悄篡改历史，那是最难查的一类 bug。
//! 3. **载荷直接复用 `model_client::HistoryItem`**：04 §3.1 的 `ResponseItem` 就是
//!    "模型请求/响应/工具调用的完整记录"，而 HistoryItem 正是循环里那份记录。再造一个
//!    同构类型 = 两份真相，resume 时必然与运行时历史漂移。
//!
//! 边界（AGENTS §3）：rollout 含用户代码与对话，属敏感数据。本模块**不**写入任何密钥；
//! `config_snapshot` 只存端点/模型/模式等非凭据字段，`base_url` 允许入库但禁止含 query 凭据。

pub mod resume;

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::model_client::HistoryItem;

/// 会话文件目录名（04 §3.1：`<data_root>/rollouts/<agent_id>.jsonl`）。
pub const ROLLOUT_DIR: &str = "rollouts";

/// 会话建档时固化的配置快照。字段全是字符串以保证**跨版本可读**：
/// 枚举改名/拆分不该让历史档案解析失败（resume 依赖它）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ConfigSnapshot {
    pub provider: String,
    pub base_url: String,
    pub model: String,
    pub sandbox: String,
    pub approval: String,
    pub workspace_root: String,
}

/// rollout 的一行。判别标签 `type` 用 snake_case，与 `EventMsg` 同一套约定（04 §1）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RolloutItem {
    /// 文件首行，必须存在（04 §3.1）。
    SessionMeta {
        session_id: String,
        agent_id: String,
        /// 根代理为 null；子代理填父 agent_id（M5 用）。
        #[serde(default)]
        parent_id: Option<String>,
        /// Unix 毫秒。
        created_at: u64,
        config_snapshot: ConfigSnapshot,
    },
    /// 模型侧的一条完整记录。刻意用命名载荷而不是 newtype：
    /// `HistoryItem` 自身是内部标签枚举，newtype 变体与之嵌套会产生 `type` 键冲突。
    ResponseItem { item: HistoryItem },
    /// 压缩锚点：`[range_start, range_end)` 区间的历史被 summary 取代（SH-032 用）。
    Compacted {
        range_start: usize,
        range_end: usize,
        summary: String,
    },
    /// 父文件中指向子代理 rollout 的引用（SH-056 用）。
    SubagentRef { child_id: String, rollout_path: String },
}

#[derive(Debug, thiserror::Error)]
pub enum RolloutError {
    #[error("rollout 目录不可用: {0}")]
    Io(String),
    #[error("rollout 文件已存在，拒绝覆盖（agent_id 必须唯一）: {0}")]
    AlreadyExists(String),
    #[error("首行不是 session_meta: {0}")]
    NoSessionMeta(String),
    #[error("rollout 第 {line} 行解析失败（中间行不容忍）: {message}")]
    Corrupted { line: usize, message: String },
}

/// 已打开的 rollout 追加写入器。`Mutex<File>` 让循环里的多处写入点能共用同一个 writer
/// 而不必关心调用方在哪个线程——工具执行与事件广播都可能落在不同任务上。
#[derive(Debug)]
pub struct RolloutWriter {
    path: PathBuf,
    file: Mutex<File>,
}

impl RolloutWriter {
    /// 建档：创建 `<dir>/<agent_id>.jsonl` 并写入首行 SessionMeta。
    /// 已存在则拒绝——覆盖历史档案是不可逆的数据丢失，绝不静默进行。
    pub fn create(dir: &Path, meta: &RolloutItem) -> Result<Self, RolloutError> {
        let (_, file_name) = match meta {
            RolloutItem::SessionMeta { agent_id, .. } => (agent_id.clone(), format!("{agent_id}.jsonl")),
            other => return Err(RolloutError::NoSessionMeta(format!("建档首行必须是 session_meta，收到 {:?}", other))),
        };
        std::fs::create_dir_all(dir).map_err(|e| RolloutError::Io(format!("{dir:?}: {e}")))?;
        let path = dir.join(&file_name);
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::AlreadyExists {
                    RolloutError::AlreadyExists(format!("{}", path.display()))
                } else {
                    RolloutError::Io(format!("{path:?}: {e}"))
                }
            })?;
        let writer = Self { path, file: Mutex::new(file) };
        writer.append(meta)?;
        Ok(writer)
    }

    /// 以追加方式重新打开已有档案（resume 用，SH-031）。
    ///
    /// 校验首行确实是 `session_meta` 才允许续写：往一个没有建档的文件里追加，
    /// 产出的是一份 resume 时才发现读不了的档案，错误被推迟到最难定位的时刻。
    pub fn open_existing(path: &Path) -> Result<Self, RolloutError> {
        let head = File::open(path)
            .map_err(|e| RolloutError::Io(format!("{path:?}: {e}")))?;
        let first_line = BufReader::new(head)
            .lines()
            .next()
            .ok_or_else(|| RolloutError::NoSessionMeta(format!("{}", path.display())))?
            .map_err(|e| RolloutError::Io(format!("{}", e)))?;
        let first: RolloutItem = serde_json::from_str(&first_line)
            .map_err(|e| RolloutError::Corrupted { line: 1, message: e.to_string() })?;
        if !matches!(first, RolloutItem::SessionMeta { .. }) {
            return Err(RolloutError::NoSessionMeta(format!("{}", path.display())));
        }
        let file = OpenOptions::new()
            .append(true)
            .open(path)
            .map_err(|e| RolloutError::Io(format!("{path:?}: {e}")))?;
        Ok(Self { path: path.to_path_buf(), file: Mutex::new(file) })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 追加一行并立即 flush（设计选择 1）。
    pub fn append(&self, item: &RolloutItem) -> Result<(), RolloutError> {
        let line = serde_json::to_string(item).map_err(|e| RolloutError::Io(format!("序列化失败: {e}")))?;
        let mut file = self.file.lock().map_err(|_| RolloutError::Io("rollout 写入锁中毒".into()))?;
        file.write_all(line.as_bytes()).map_err(|e| RolloutError::Io(format!("{}", e)))?;
        file.write_all(b"\n").map_err(|e| RolloutError::Io(format!("{}", e)))?;
        file.flush().map_err(|e| RolloutError::Io(format!("flush 失败: {e}")))
    }
}

/// 读回来的档案：`items` 含首行 meta 之后的全部条目，`torn_tail_lines` 记录被丢弃的半截写行数。
#[derive(Debug, Clone, PartialEq)]
pub struct Rollout {
    pub meta: RolloutItem,
    pub items: Vec<RolloutItem>,
    /// 末行不完整（崩溃时正在写）时被丢弃的行数。正常应为 0 或 1。
    pub torn_tail_lines: usize,
}

/// 读取档案。规则见文件头设计选择 2：只容忍最后一行撕裂，中间坏行直接报错。
pub fn read(path: &Path) -> Result<Rollout, RolloutError> {
    let file = File::open(path).map_err(|e| RolloutError::Io(format!("{path:?}: {e}")))?;
    let reader = BufReader::new(file);
    let mut raw_lines: Vec<String> = reader.lines().map(|l| l.unwrap_or_default()).collect();

    // 半截写判定看**文件末尾有没有换行**，而不是看 JSON 能不能解析：
    // 崩溃时最后一段可能恰好是合法 JSON 的前缀，也可能碰巧完整——只有换行位置是可靠信号。
    let mut torn = 0usize;
    if !ends_newline(path) {
        match raw_lines.last().map(|l| l.trim().is_empty()) {
            Some(false) => {
                torn = 1;
                raw_lines.pop();
            }
            Some(true) => {
                raw_lines.pop();
            }
            None => {}
        }
    }

    let mut parsed: Vec<(usize, RolloutItem)> = Vec::new();
    for (idx, line) in raw_lines.iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let item: RolloutItem = serde_json::from_str(line)
            .map_err(|e| RolloutError::Corrupted { line: idx + 1, message: e.to_string() })?;
        parsed.push((idx + 1, item));
    }

    let (first_line, meta) = parsed
        .first()
        .cloned()
        .ok_or_else(|| RolloutError::NoSessionMeta("文件为空".into()))?;
    if !matches!(meta, RolloutItem::SessionMeta { .. }) {
        return Err(RolloutError::NoSessionMeta(format!("第 {} 行", first_line)));
    }
    let items = parsed.into_iter().skip(1).map(|(_, item)| item).collect();
    Ok(Rollout { meta, items, torn_tail_lines: torn })
}

fn ends_newline(path: &Path) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = File::open(path) else { return true };
    let Ok(len) = f.metadata().map(|m| m.len()) else { return true };
    if len == 0 {
        return true;
    }
    if f.seek(SeekFrom::End(-1)).is_err() {
        return true;
    }
    let mut buf = [0u8; 1];
    f.read_exact(&mut buf).map(|_| buf[0] == b'\n').unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_client::{HistoryItem, Role};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static SEQ: AtomicUsize = AtomicUsize::new(0);

    fn temp_dir() -> PathBuf {
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("shaniu-rollout-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn meta(agent: &str) -> RolloutItem {
        RolloutItem::SessionMeta {
            session_id: "sess-1".into(),
            agent_id: agent.into(),
            parent_id: None,
            created_at: 1_700_000_000_000,
            config_snapshot: ConfigSnapshot {
                model: "m".into(),
                base_url: "https://x/v1".into(),
                ..Default::default()
            },
        }
    }

    #[test]
    fn create_writes_session_meta_as_first_line() {
        let dir = temp_dir();
        let agent = "agent-first";
        let w = RolloutWriter::create(&dir, &meta(agent)).unwrap();
        let raw = std::fs::read_to_string(w.path()).unwrap();
        let first = raw.lines().next().unwrap();
        let v: serde_json::Value = serde_json::from_str(first).unwrap();
        assert_eq!(v["type"], "session_meta", "首行必须是 session_meta 判别标签");
        assert_eq!(v["agent_id"], agent);
        assert_eq!(v["config_snapshot"]["base_url"], "https://x/v1");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn refuses_to_overwrite_existing_agent_file() {
        let dir = temp_dir();
        let _w = RolloutWriter::create(&dir, &meta("dup")).unwrap();
        let err = RolloutWriter::create(&dir, &meta("dup")).unwrap_err();
        assert!(matches!(err, RolloutError::AlreadyExists(_)), "覆盖历史档案不可逆，必须拒绝：{err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn round_trip_preserves_items_in_order() {
        let dir = temp_dir();
        let w = RolloutWriter::create(&dir, &meta("agent-rt")).unwrap();
        let items = vec![
            RolloutItem::ResponseItem { item: HistoryItem::Message { role: Role::User, text: "任务".into(), reasoning: None } },
            RolloutItem::ResponseItem { item: HistoryItem::FunctionCall { name: "read_file".into(), arguments: "{}".into(), call_id: "c1".into() } },
            RolloutItem::ResponseItem { item: HistoryItem::FunctionCallOutput { call_id: "c1".into(), output: "内容".into() } },
            RolloutItem::Compacted { range_start: 0, range_end: 2, summary: "前两轮摘要".into() },
            RolloutItem::SubagentRef { child_id: "child-1".into(), rollout_path: "rollouts/child-1.jsonl".into() },
        ];
        for it in &items {
            w.append(it).unwrap();
        }
        let back = read(&dir.join("agent-rt.jsonl")).unwrap();
        assert_eq!(back.meta, meta("agent-rt"));
        assert_eq!(back.items, items, "读回来的条目序列必须与写入完全一致");
        assert_eq!(back.torn_tail_lines, 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tolerates_torn_last_line_but_rejects_middle_corruption() {
        let dir = temp_dir();
        let path = dir.join("agent-torn.jsonl");
        // 崩溃在写第三行的中途：末行没有换行且不是合法 JSON
        std::fs::write(
            &path,
            format!("{}\n{}\n{{\"type\":\"response_item\"",
                serde_json::to_string(&meta("t")).unwrap(),
                serde_json::to_string(&RolloutItem::ResponseItem {
                    item: HistoryItem::Message { role: Role::Assistant, text: "答".into(), reasoning: None },
                })
                .unwrap(),
            ),
        )
        .unwrap();
        let back = read(&path).unwrap();
        assert_eq!(back.items.len(), 1, "半截写的那行应被丢弃");
        assert_eq!(back.torn_tail_lines, 1);

        // 同样的坏内容出现在中间（后面还有完整行）→ 必须报错，不能悄悄跳过
        std::fs::write(
            &path,
            format!("{}\n{{\"type\":\"bogus\"\n{}\n",
                serde_json::to_string(&meta("t")).unwrap(),
                serde_json::to_string(&RolloutItem::ResponseItem {
                    item: HistoryItem::Message { role: Role::Assistant, text: "答".into(), reasoning: None },
                })
                .unwrap(),
            ),
        )
        .unwrap();
        let err = read(&path).unwrap_err();
        assert!(matches!(err, RolloutError::Corrupted { .. }), "中间坏行必须报错：{err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_or_wrong_first_line_is_rejected() {
        let dir = temp_dir();
        let empty = dir.join("agent-empty.jsonl");
        std::fs::write(&empty, "").unwrap();
        assert!(matches!(read(&empty).unwrap_err(), RolloutError::NoSessionMeta(_)));

        let no_meta = dir.join("agent-nometa.jsonl");
        std::fs::write(
            &no_meta,
            format!("{}\n",
                serde_json::to_string(&RolloutItem::ResponseItem {
                    item: HistoryItem::Message { role: Role::User, text: "x".into(), reasoning: None },
                })
                .unwrap()),
        )
        .unwrap();
        assert!(matches!(read(&no_meta).unwrap_err(), RolloutError::NoSessionMeta(_)));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn create_rejects_non_meta_first_item() {
        let dir = temp_dir();
        let err = RolloutWriter::create(
            &dir,
            &RolloutItem::ResponseItem {
                item: HistoryItem::Message { role: Role::User, text: "x".into(), reasoning: None },
            },
        )
        .unwrap_err();
        assert!(matches!(err, RolloutError::NoSessionMeta(_)), "建档只接受 session_meta：{err}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
