//! 后台作业注册表（SH-089）：进程内的 `JobView` 投影与有界输出环。
//!
//! 对齐上游 `packages/jobs/jobs` 的客户端可见语义：
//! - 作业有稳定 id（`<kind>-<短 id>`）、生命周期 `running → stopping → 终态`；
//! - 输出按 UTF-8 字节绝对偏移累积，环形缓冲只丢头部，`earliest` 单调前进；
//! - 读取不消费（`job_read` 从任意偏移取回保留区），供轮询式观察流使用；
//! - 人类/模型终止走同一入口：置 `stopping` 并通知 worker，终态由 worker 落定。
//!
//! 与上游的差异（如实声明）：
//! - 上游把作业写进会话日志并由 Host 推流；桌面壳的档案面没有作业事件槽位，
//!   所以注册表只活在进程内存，进程退出即清空（`docs/05` SH-089 注）；
//! - 上游 `job.list`/`job.follow` 是推送流；适配层用 `job_list`/`job_read` 轮询
//!   还原同名帧序列，语义（整表替换 / opened-output-status）保持一致。

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tauri::Manager;
use tokio::sync::oneshot;

use crate::app_state::AppState;

/// 输出环保留上限（UTF-8 字节）。与前端渲染尾部 128KiB 对齐后留一倍余量。
pub const RING_CAP_BYTES: usize = 256 * 1024;

/// 作业命令失败的结构化回执；`code` 与上游错误码同名。
#[derive(Debug, Serialize)]
pub struct JobOpError {
    pub code: String,
    pub message: String,
}

impl JobOpError {
    fn new(code: &str, message: impl Into<String>) -> Self {
        Self { code: code.to_string(), message: message.into() }
    }
}

/// `JobView`：客户端可见的作业投影（wire 形状与上游一致）。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobView {
    pub id: String,
    pub kind: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub started_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<u64>,
    pub output: JobOutputView,
}

/// 输出环的绝对坐标。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobOutputView {
    pub total: u64,
    pub earliest: u64,
}

/// 一段输出；`gapBefore` 只在确实丢过字节时出现。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobChunk {
    pub at: u64,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gap_before: Option<bool>,
}

/// `job_read` 的返回：当前投影 + 从请求偏移起的保留区。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobRead {
    pub job: JobView,
    pub chunks: Vec<JobChunk>,
    pub next: u64,
    pub lossy: bool,
}

/// `job_kill` 的回执。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobKillValue {
    pub outcome: String,
}

#[derive(Debug, Clone)]
struct RingChunk {
    at: u64,
    text: String,
    bytes: u64,
    channel: Option<String>,
    gap_before: bool,
}

/// 有界输出环：偏移一旦分配就不再移动，丢掉头部只推进 `earliest`。
#[derive(Debug, Default)]
struct Ring {
    chunks: Vec<RingChunk>,
    retained: u64,
    total: u64,
    earliest: u64,
}

impl Ring {
    fn append(&mut self, text: &str, channel: Option<&str>) {
        if text.is_empty() {
            return;
        }
        let bytes = text.len() as u64;
        self.chunks.push(RingChunk {
            at: self.total,
            text: text.to_string(),
            bytes,
            channel: channel.map(str::to_string),
            gap_before: false,
        });
        self.total += bytes;
        self.retained += bytes;
        self.trim(RING_CAP_BYTES);
    }

    fn trim(&mut self, cap: usize) {
        while self.retained > cap as u64 && self.chunks.len() > 1 {
            let dropped = self.chunks.remove(0);
            self.retained -= dropped.bytes;
        }
        if self.chunks.len() == 1 && self.chunks[0].bytes > cap as u64 {
            let text = std::mem::take(&mut self.chunks[0].text);
            // 从尾部取 cap 字节，并把切点推进到字符边界，避免半个 UTF-8 码点。
            let mut start = text.len().saturating_sub(cap);
            while start < text.len() && !text.is_char_boundary(start) {
                start += 1;
            }
            let tail = text[start..].to_string();
            let dropped = (text.len() - tail.len()) as u64;
            let chunk = &mut self.chunks[0];
            chunk.at += dropped;
            chunk.bytes = tail.len() as u64;
            chunk.text = tail;
            chunk.gap_before = true;
            self.retained = chunk.bytes;
        }
        self.earliest = self.chunks.first().map(|chunk| chunk.at).unwrap_or(self.total);
    }

    fn read_from(&self, from: u64) -> (Vec<JobChunk>, u64, bool) {
        let chunks: Vec<JobChunk> = self
            .chunks
            .iter()
            .filter(|chunk| chunk.at + chunk.bytes > from)
            .map(|chunk| JobChunk {
                at: chunk.at,
                text: chunk.text.clone(),
                channel: chunk.channel.clone(),
                gap_before: chunk.gap_before.then_some(true),
            })
            .collect();
        (chunks, self.total, from < self.earliest)
    }
}

#[derive(Debug)]
struct JobEntry {
    id: String,
    kind: String,
    label: String,
    owner: Option<String>,
    status: &'static str,
    progress: Option<String>,
    detail: Option<String>,
    started_at: u64,
    finished_at: Option<u64>,
    ring: Ring,
    /// 终止信号；worker 取走后这里留 None。
    kill: Option<oneshot::Sender<()>>,
}

impl JobEntry {
    fn view(&self) -> JobView {
        JobView {
            id: self.id.clone(),
            kind: self.kind.clone(),
            label: self.label.clone(),
            owner: self.owner.clone(),
            status: self.status.to_string(),
            progress: self.progress.clone(),
            detail: self.detail.clone(),
            started_at: self.started_at,
            finished_at: self.finished_at,
            output: JobOutputView { total: self.ring.total, earliest: self.ring.earliest },
        }
    }

    /// 会话围栏：无主作业对任何会话可见，有主作业只对属主可见。
    fn visible_to(&self, session_id: &str) -> bool {
        self.owner.as_deref().map(|owner| owner == session_id).unwrap_or(true)
    }
}

#[derive(Debug, Default)]
struct RegistryInner {
    seq: u64,
    jobs: BTreeMap<String, JobEntry>,
}

/// 进程级作业注册表；由 `AppState` 持有，`AgentLoop` 交给工具上下文。
#[derive(Debug, Default)]
pub struct JobRegistry {
    inner: Mutex<RegistryInner>,
}

impl JobRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, RegistryInner> {
        self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 登记一个运行中的作业，返回 id 与终止信号接收端。
    pub fn create(&self, kind: &str, label: &str, owner: Option<String>) -> (String, oneshot::Receiver<()>) {
        let (tx, rx) = oneshot::channel();
        let mut guard = self.lock();
        guard.seq += 1;
        // `<kind>-<启动毫秒低 32 位><序号>`：进程内唯一，形状与上游 `<kind>-<短 id>` 一致。
        let id = format!("{kind}-{:08x}{:04x}", now_ms() as u32, guard.seq as u16);
        let entry = JobEntry {
            id: id.clone(),
            kind: kind.to_string(),
            label: label.to_string(),
            owner,
            status: "running",
            progress: None,
            detail: None,
            started_at: now_ms(),
            finished_at: None,
            ring: Ring::default(),
            kill: Some(tx),
        };
        guard.jobs.insert(id.clone(), entry);
        (id, rx)
    }

    /// 追加一段输出（worker 调用；行尾已由调用方补全）。
    pub fn append(&self, id: &str, text: &str, channel: Option<&str>) {
        let mut guard = self.lock();
        if let Some(entry) = guard.jobs.get_mut(id) {
            entry.ring.append(text, channel);
        }
    }

    /// 落定终态；重复调用只保留第一次（终态不可回退）。
    pub fn finish(&self, id: &str, status: &'static str, detail: Option<String>) {
        let mut guard = self.lock();
        let Some(entry) = guard.jobs.get_mut(id) else { return };
        if matches!(entry.status, "completed" | "killed" | "failed") {
            return;
        }
        entry.status = status;
        if let Some(detail) = detail {
            entry.detail = Some(detail);
        }
        entry.finished_at = Some(now_ms());
        entry.kill = None;
    }

    /// 会话可见的作业整表（含无主作业），按开始时间升序。
    pub fn list_for(&self, session_id: &str) -> Vec<JobView> {
        let guard = self.lock();
        let mut jobs: Vec<JobView> = guard
            .jobs
            .values()
            .filter(|entry| entry.visible_to(session_id))
            .map(JobEntry::view)
            .collect();
        jobs.sort_by_key(|job| job.started_at);
        jobs
    }

    /// 全部未终态作业（退出前检查用；跨会话）。按开始时间升序。
    pub fn list_running(&self) -> Vec<JobView> {
        let guard = self.lock();
        let mut jobs: Vec<JobView> = guard
            .jobs
            .values()
            .filter(|entry| !is_terminal(entry.status))
            .map(JobEntry::view)
            .collect();
        jobs.sort_by_key(|job| job.started_at);
        jobs
    }

    /// 退出路径专用：不按会话校验归属，直接请求终止。
    /// 返回是否真的发出了停止信号（已终态返回 false）。
    pub fn kill_unscoped(&self, id: &str) -> bool {
        let mut guard = self.lock();
        match guard.jobs.get_mut(id) {
            Some(entry) if !is_terminal(entry.status) => {
                entry.status = "stopping";
                if let Some(signal) = entry.kill.take() {
                    let _ = signal.send(());
                }
                true
            }
            _ => false,
        }
    }

    /// 非消费读取：返回从 `from` 起仍保留的字节；`from` 缺省取 `earliest`。
    pub fn read(&self, id: &str, session_id: &str, from: Option<u64>) -> Result<JobRead, JobOpError> {
        let guard = self.lock();
        let entry = guard.jobs.get(id).ok_or_else(|| not_found(session_id, id))?;
        if !entry.visible_to(session_id) {
            return Err(not_found(session_id, id));
        }
        let from = from.unwrap_or(entry.ring.earliest);
        let (chunks, next, lossy) = entry.ring.read_from(from);
        Ok(JobRead { job: entry.view(), chunks, next, lossy })
    }

    /// 请求终止；运行中置 `stopping` 并通知 worker，已结束如实返回。
    pub fn kill(&self, id: &str, session_id: &str) -> Result<JobKillValue, JobOpError> {
        let mut guard = self.lock();
        let entry = guard.jobs.get_mut(id).ok_or_else(|| not_found(session_id, id))?;
        if !entry.visible_to(session_id) {
            return Err(not_found(session_id, id));
        }
        match entry.status {
            "running" => {
                entry.status = "stopping";
                if let Some(signal) = entry.kill.take() {
                    let _ = signal.send(());
                }
                Ok(JobKillValue { outcome: "requested".into() })
            }
            "stopping" => Ok(JobKillValue { outcome: "requested".into() }),
            _ => Ok(JobKillValue { outcome: "already-finished".into() }),
        }
    }
}

fn not_found(session_id: &str, job_id: &str) -> JobOpError {
    JobOpError::new(
        "job/not-found",
        format!("会话 {session_id} 的作业列表中没有 {job_id}"),
    )
}

/// 终态判定：只有这三个值不再算“在跑的事”。
fn is_terminal(status: &str) -> bool {
    matches!(status, "completed" | "killed" | "failed")
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

// ============================== Tauri 命令 ==============================

/// 某会话可见的作业整表（`job.list` 的单帧来源）。
#[tauri::command]
pub fn job_list(app: tauri::AppHandle, session_id: String) -> Vec<JobView> {
    app.state::<AppState>().jobs.list_for(&session_id)
}

/// 非消费读取（`job.follow` 的轮询来源）。
#[tauri::command]
pub fn job_read(
    app: tauri::AppHandle,
    session_id: String,
    job_id: String,
    from: Option<u64>,
) -> Result<JobRead, JobOpError> {
    app.state::<AppState>().jobs.read(&job_id, &session_id, from)
}

/// 请求终止一个后台作业。
#[tauri::command]
pub fn job_kill(
    app: tauri::AppHandle,
    session_id: String,
    job_id: String,
) -> Result<JobKillValue, JobOpError> {
    app.state::<AppState>().jobs.kill(&job_id, &session_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry_with_job() -> (JobRegistry, String, oneshot::Receiver<()>) {
        let registry = JobRegistry::new();
        let (id, rx) = registry.create("bash", "echo hello", Some("s1".into()));
        (registry, id, rx)
    }

    #[test]
    fn append_and_read_preserve_absolute_offsets() {
        let (registry, id, _rx) = registry_with_job();
        registry.append(&id, "one\n", Some("stdout"));
        registry.append(&id, "two\n", Some("stderr"));
        let read = registry.read(&id, "s1", None).unwrap();
        assert_eq!(read.job.status, "running");
        assert_eq!(read.job.output.total, 8);
        assert_eq!(read.cursor(), 8);
        assert_eq!(read.chunks.len(), 2);
        assert_eq!(read.chunks[0].at, 0);
        assert_eq!(read.chunks[1].at, 4);
        assert_eq!(read.chunks[1].channel.as_deref(), Some("stderr"));
        // 从第二段开始读只返回与区间相交的块，偏移不移动。
        let tail = registry.read(&id, "s1", Some(4)).unwrap();
        assert_eq!(tail.chunks.len(), 1);
        assert_eq!(tail.chunks[0].text, "two\n");
    }

    #[test]
    fn ring_evicts_head_and_reports_gap() {
        let (registry, id, _rx) = registry_with_job();
        let line = "x".repeat(1024);
        for _ in 0..(RING_CAP_BYTES / 1024 + 8) {
            registry.append(&id, &line, Some("stdout"));
        }
        let read = registry.read(&id, "s1", None).unwrap();
        assert_eq!(read.job.output.total, (RING_CAP_BYTES as u64 / 1024 + 8) * 1024);
        assert!(read.job.output.earliest > 0);
        assert!(read.lossy_past_head());
        // 从 0 读取会标记 lossy，因为头部已被丢弃。
        let from_zero = registry.read(&id, "s1", Some(0)).unwrap();
        assert!(from_zero.lossy);
    }

    #[test]
    fn settle_is_terminal_and_kill_reports_finished() {
        let (registry, id, _rx) = registry_with_job();
        registry.finish(&id, "completed", Some("exit code: 0".into()));
        registry.finish(&id, "failed", None);
        let read = registry.read(&id, "s1", None).unwrap();
        assert_eq!(read.job.status, "completed");
        assert_eq!(read.job.detail.as_deref(), Some("exit code: 0"));
        assert!(read.job.finished_at.is_some());
        let killed = registry.kill(&id, "s1").unwrap();
        assert_eq!(killed.outcome, "already-finished");
    }

    #[tokio::test]
    async fn kill_running_job_moves_to_stopping_and_signals() {
        let (registry, id, mut rx) = registry_with_job();
        let value = registry.kill(&id, "s1").unwrap();
        assert_eq!(value.outcome, "requested");
        assert_eq!(registry.read(&id, "s1", None).unwrap().job.status, "stopping");
        assert!(rx.try_recv().is_ok());
        // 第二次请求仍被接受，终态由 worker 落定。
        assert_eq!(registry.kill(&id, "s1").unwrap().outcome, "requested");
        registry.finish(&id, "killed", Some("kill requested".into()));
        assert_eq!(registry.read(&id, "s1", None).unwrap().job.status, "killed");
    }

    #[test]
    fn list_and_read_are_fenced_by_owner() {
        let registry = JobRegistry::new();
        let (owned, _r1) = registry.create("bash", "owned", Some("s1".into()));
        let (unowned, _r2) = registry.create("bash", "unowned", None);
        let ids: Vec<String> = registry.list_for("s1").into_iter().map(|job| job.id).collect();
        assert_eq!(ids, vec![owned.clone(), unowned.clone()]);
        assert!(registry.list_for("s2").iter().all(|job| job.id == unowned));
        assert!(matches!(
            registry.read(&owned, "s2", None).unwrap_err().code.as_str(),
            "job/not-found"
        ));
        assert_eq!(registry.read(&unowned, "s2", None).unwrap().job.label, "unowned");
    }

    #[test]
    fn missing_job_is_not_found() {
        let registry = JobRegistry::new();
        let error = registry.read("bash-nope", "s1", None).unwrap_err();
        assert_eq!(error.code, "job/not-found");
        assert_eq!(registry.kill("bash-nope", "s1").unwrap_err().code, "job/not-found");
    }

    impl JobRead {
        fn cursor(&self) -> u64 {
            self.next
        }

        fn lossy_past_head(&self) -> bool {
            self.job.output.earliest > 0
        }
    }
}
