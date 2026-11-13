//! 会话目标域（SH-088）：`ui-goal` 目标条的宿主持久层。
//!
//! 上游 `packages/goal/goal` 把目标写成会话日志里的 `goal/change` 事件，由严格
//! fold 重建投影；桌面壳的档案面（rollout）只认模型对话条目，没有目标事件槽位。
//! 因此这里把**同一份 wire 形状**（`GoalProjection`）落到
//! `<data_root>/goals/<session_id>.json`：读取即得到投影值，写入做 CAS 校验。
//!
//! 语义对齐上游：
//! - 每个目标有稳定 `id` 与单调递增 `revision`，所有变更都带 CAS（`goal_id` + `revision`）；
//! - `create` 只允许在没有目标或上一个目标已 `complete` 时发生；
//! - `edit` 只改 objective / maxGoalRounds，不碰 phase；
//! - `pause` 只接受 active，`resume` 接受 active/paused/blocked 且轮次预算未耗尽；
//! - `clear` 留下一条 `revision + 1` 的墓碑 ref，并保留 `seenGoalIds` 防复用。
//!
//! **进程内 activation 不在这里**：上游把 armed/disarmed 放在宿主进程内存里、永不
//! 持久化；桌面壳由适配层（`src/transport/adapter.ts`）持有同一份进程内状态，
//! 本模块只负责 durable 投影。

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::Manager;

use crate::app_state::AppState;
use crate::clock::now_ms;

/// 目标目录名（相对 `data_root`）。
pub const GOALS_DIR: &str = "goals";
/// 创建目标时缺省的轮次上限，与上游 `GoalService.Config.defaultMaxGoalRounds` 同值。
pub const DEFAULT_MAX_GOAL_ROUNDS: u64 = 256;

/// 目标变更失败的结构化回执；`code` 与上游 `GoalErrorCode` 同名。
#[derive(Debug, Serialize)]
pub struct GoalOpError {
    pub code: String,
    pub message: String,
}

impl GoalOpError {
    fn new(code: &str, message: impl Into<String>) -> Self {
        Self { code: code.to_string(), message: message.into() }
    }
}

/// 目标受阻原因（仅 `blocked` 阶段携带）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoalBlockReason {
    pub code: String,
    pub message: String,
}

/// 一份完整的目标快照；`revision` 是 CAS 身份的一部分。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoalSnapshot {
    pub id: String,
    pub revision: u64,
    pub objective: String,
    /// `active` / `paused` / `blocked` / `complete`。
    pub phase: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_reason: Option<GoalBlockReason>,
    pub max_goal_rounds: u64,
}

/// `goal` 投影值（`GoalProjection`）：当前目标 + 回放计数与时间戳。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoalProjection {
    pub goal: GoalSnapshot,
    pub rounds_started: u64,
    pub created_at: u64,
    pub updated_at: u64,
}

/// 精确到某一版目标的 CAS 引用。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoalRef {
    pub id: String,
    pub revision: u64,
}

/// 磁盘上的会话目标状态（等价上游 `GoalProjectionState` 的 durable 部分）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GoalStore {
    #[serde(default)]
    current: Option<GoalProjection>,
    #[serde(default)]
    seen_goal_ids: Vec<String>,
}

/// 单进程内的读改写闸：Tauri 命令可能并发到达，CAS 必须基于同一份最新状态判定。
static GOAL_LOCK: Mutex<()> = Mutex::new(());

/// 会话 id 只允许档案面同一组安全字符，避免路径穿越。
fn goal_path(data_root: &Path, session_id: &str) -> Result<PathBuf, GoalOpError> {
    if session_id.is_empty()
        || !session_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(GoalOpError::new("GOAL_AGENT_NOT_LIVE", format!("非法会话 id：{session_id}")));
    }
    Ok(data_root.join(GOALS_DIR).join(format!("{session_id}.json")))
}

fn load(path: &Path) -> Result<GoalStore, GoalOpError> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| {
            GoalOpError::new("GOAL_INVALID_STATE", format!("目标状态读取失败（{}）：{e}", path.display()))
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(GoalStore::default()),
        Err(error) => Err(GoalOpError::new(
            "GOAL_INVALID_STATE",
            format!("目标状态读取失败（{}）：{error}", path.display()),
        )),
    }
}

/// 原子写：先写同目录临时文件再 rename，崩溃时不会留下半份 JSON。
fn save(path: &Path, store: &GoalStore) -> Result<(), GoalOpError> {
    let dir = path
        .parent()
        .ok_or_else(|| GoalOpError::new("GOAL_INVALID_STATE", "目标路径没有父目录"))?;
    std::fs::create_dir_all(dir).map_err(|e| {
        GoalOpError::new("GOAL_INVALID_STATE", format!("目标目录创建失败（{}）：{e}", dir.display()))
    })?;
    let bytes = serde_json::to_vec_pretty(store)
        .map_err(|e| GoalOpError::new("GOAL_INVALID_STATE", format!("目标状态序列化失败：{e}")))?;
    let tmp = crate::config::temp_sibling(path);
    std::fs::write(&tmp, bytes).map_err(|e| {
        GoalOpError::new("GOAL_INVALID_STATE", format!("目标状态写入失败（{}）：{e}", tmp.display()))
    })?;
    std::fs::rename(&tmp, path).map_err(|e| {
        GoalOpError::new("GOAL_INVALID_STATE", format!("目标状态落盘失败（{}）：{e}", path.display()))
    })
}

fn resolve_objective(value: &str) -> Result<String, GoalOpError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(GoalOpError::new("GOAL_INVALID_OBJECTIVE", "目标内容不能为空"));
    }
    Ok(trimmed.to_string())
}

fn resolve_max_rounds(value: u64) -> Result<u64, GoalOpError> {
    if value < 1 {
        return Err(GoalOpError::new("GOAL_INVALID_MAX_ROUNDS", "maxGoalRounds 必须是正整数"));
    }
    Ok(value)
}

fn expect_current(store: &GoalStore, goal_id: &str, revision: u64) -> Result<GoalProjection, GoalOpError> {
    let current = store
        .current
        .clone()
        .ok_or_else(|| GoalOpError::new("GOAL_NOT_FOUND", "当前没有目标"))?;
    if current.goal.id != goal_id || current.goal.revision != revision {
        return Err(GoalOpError::new(
            "GOAL_STALE_REVISION",
            format!(
                "目标引用已过期（请求 {} revision {}，当前 {} revision {}）",
                goal_id, revision, current.goal.id, current.goal.revision
            ),
        ));
    }
    Ok(current)
}

fn with_phase(current: &GoalSnapshot, phase: &str) -> GoalSnapshot {
    GoalSnapshot {
        id: current.id.clone(),
        revision: current.revision + 1,
        objective: current.objective.clone(),
        phase: phase.to_string(),
        blocked_reason: None,
        max_goal_rounds: current.max_goal_rounds,
    }
}

fn next_mutation_time(current: &GoalProjection) -> u64 {
    now_ms().max(current.updated_at)
}

// ---- 纯状态机（便于单测；命令层只负责路径与锁）----

fn create_in(store: &mut GoalStore, objective: &str, max_goal_rounds: Option<u64>) -> Result<GoalProjection, GoalOpError> {
    if let Some(current) = &store.current {
        if current.goal.phase != "complete" {
            return Err(GoalOpError::new(
                "GOAL_ALREADY_EXISTS",
                format!("目标 {} 已存在（{}）", current.goal.id, current.goal.phase),
            ));
        }
    }
    let objective = resolve_objective(objective)?;
    let max_goal_rounds = resolve_max_rounds(max_goal_rounds.unwrap_or(DEFAULT_MAX_GOAL_ROUNDS))?;
    let mut id = format!("goal-{}", ulid::Ulid::generate());
    while store.seen_goal_ids.iter().any(|seen| seen == &id) {
        id = format!("goal-{}", ulid::Ulid::generate());
    }
    let now = now_ms();
    let projection = GoalProjection {
        goal: GoalSnapshot {
            id: id.clone(),
            revision: 1,
            objective,
            phase: "active".to_string(),
            blocked_reason: None,
            max_goal_rounds,
        },
        rounds_started: 0,
        created_at: now,
        updated_at: now,
    };
    store.seen_goal_ids.push(id);
    store.current = Some(projection.clone());
    Ok(projection)
}

fn edit_in(
    store: &mut GoalStore,
    goal_id: &str,
    revision: u64,
    objective: Option<&str>,
    max_goal_rounds: Option<u64>,
) -> Result<GoalProjection, GoalOpError> {
    let current = expect_current(store, goal_id, revision)?;
    if objective.is_none() && max_goal_rounds.is_none() {
        return Err(GoalOpError::new("GOAL_INVALID_EDIT", "目标编辑至少需要 objective 或 maxGoalRounds"));
    }
    let objective = match objective {
        Some(value) => resolve_objective(value)?,
        None => current.goal.objective.clone(),
    };
    let max_goal_rounds = match max_goal_rounds {
        Some(value) => resolve_max_rounds(value)?,
        None => current.goal.max_goal_rounds,
    };
    let next = GoalProjection {
        goal: GoalSnapshot {
            revision: current.goal.revision + 1,
            objective,
            max_goal_rounds,
            ..current.goal.clone()
        },
        rounds_started: current.rounds_started,
        created_at: current.created_at,
        updated_at: next_mutation_time(&current),
    };
    store.current = Some(next.clone());
    Ok(next)
}

fn transition_in(
    store: &mut GoalStore,
    goal_id: &str,
    revision: u64,
    allowed: &[&str],
    phase: &str,
) -> Result<GoalProjection, GoalOpError> {
    let current = expect_current(store, goal_id, revision)?;
    if !allowed.contains(&current.goal.phase.as_str()) {
        return Err(GoalOpError::new(
            "GOAL_INVALID_TRANSITION",
            format!(
                "目标 {} 当前为 {}，不能转换到 {}（允许：{}）",
                current.goal.id,
                current.goal.phase,
                phase,
                allowed.join(" / ")
            ),
        ));
    }
    let next = GoalProjection {
        goal: with_phase(&current.goal, phase),
        rounds_started: current.rounds_started,
        created_at: current.created_at,
        updated_at: next_mutation_time(&current),
    };
    store.current = Some(next.clone());
    Ok(next)
}

fn resume_in(store: &mut GoalStore, goal_id: &str, revision: u64) -> Result<GoalProjection, GoalOpError> {
    let current = expect_current(store, goal_id, revision)?;
    if !matches!(current.goal.phase.as_str(), "active" | "paused" | "blocked") {
        return Err(GoalOpError::new(
            "GOAL_INVALID_TRANSITION",
            format!("目标 {} 当前为 {}，不能恢复", current.goal.id, current.goal.phase),
        ));
    }
    if current.rounds_started >= current.goal.max_goal_rounds {
        return Err(GoalOpError::new(
            "GOAL_INVALID_TRANSITION",
            format!(
                "目标 {} 已用尽 {} 轮预算；先提高 maxGoalRounds 再恢复",
                current.goal.id, current.goal.max_goal_rounds
            ),
        ));
    }
    let next = GoalProjection {
        goal: with_phase(&current.goal, "active"),
        rounds_started: current.rounds_started,
        created_at: current.created_at,
        updated_at: next_mutation_time(&current),
    };
    store.current = Some(next.clone());
    Ok(next)
}

fn clear_in(store: &mut GoalStore, goal_id: &str, revision: u64) -> Result<GoalRef, GoalOpError> {
    let current = expect_current(store, goal_id, revision)?;
    let tombstone = GoalRef { id: current.goal.id, revision: current.goal.revision + 1 };
    store.current = None;
    Ok(tombstone)
}

// ---- Tauri 命令面 ----

fn store_of(app: &tauri::AppHandle, session_id: &str) -> Result<(PathBuf, GoalStore), GoalOpError> {
    let state = app.state::<AppState>();
    let path = goal_path(&state.data_root(), session_id)?;
    let store = load(&path)?;
    Ok((path, store))
}

/// 读取会话当前目标投影；没有目标时返回 `None`（wire 上是 `goal: null`）。
#[tauri::command]
pub fn goal_get(app: tauri::AppHandle, session_id: String) -> Result<Option<GoalProjection>, GoalOpError> {
    let (_, store) = store_of(&app, &session_id)?;
    Ok(store.current)
}

/// 创建目标（仅当没有目标或上一个目标已完成）。
#[tauri::command]
pub fn goal_create(
    app: tauri::AppHandle,
    session_id: String,
    objective: String,
    max_goal_rounds: Option<u64>,
) -> Result<GoalProjection, GoalOpError> {
    let _guard = GOAL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (path, mut store) = store_of(&app, &session_id)?;
    let projection = create_in(&mut store, &objective, max_goal_rounds)?;
    save(&path, &store)?;
    Ok(projection)
}

/// 编辑目标内容 / 轮次上限（不改 phase）。
#[tauri::command]
pub fn goal_edit(
    app: tauri::AppHandle,
    session_id: String,
    goal_id: String,
    revision: u64,
    objective: Option<String>,
    max_goal_rounds: Option<u64>,
) -> Result<GoalProjection, GoalOpError> {
    let _guard = GOAL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (path, mut store) = store_of(&app, &session_id)?;
    let projection = edit_in(&mut store, &goal_id, revision, objective.as_deref(), max_goal_rounds)?;
    save(&path, &store)?;
    Ok(projection)
}

/// 暂停 active 目标。
#[tauri::command]
pub fn goal_pause(
    app: tauri::AppHandle,
    session_id: String,
    goal_id: String,
    revision: u64,
) -> Result<GoalProjection, GoalOpError> {
    let _guard = GOAL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (path, mut store) = store_of(&app, &session_id)?;
    let projection = transition_in(&mut store, &goal_id, revision, &["active"], "paused")?;
    save(&path, &store)?;
    Ok(projection)
}

/// 恢复 active / paused / blocked 目标（轮次预算未耗尽）。
#[tauri::command]
pub fn goal_resume(
    app: tauri::AppHandle,
    session_id: String,
    goal_id: String,
    revision: u64,
) -> Result<GoalProjection, GoalOpError> {
    let _guard = GOAL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (path, mut store) = store_of(&app, &session_id)?;
    let projection = resume_in(&mut store, &goal_id, revision)?;
    save(&path, &store)?;
    Ok(projection)
}

/// 清除当前目标，返回 revision + 1 的墓碑 ref。
#[tauri::command]
pub fn goal_clear(
    app: tauri::AppHandle,
    session_id: String,
    goal_id: String,
    revision: u64,
) -> Result<GoalRef, GoalOpError> {
    let _guard = GOAL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (path, mut store) = store_of(&app, &session_id)?;
    let tombstone = clear_in(&mut store, &goal_id, revision)?;
    save(&path, &store)?;
    Ok(tombstone)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> GoalStore {
        GoalStore::default()
    }

    fn seed(store: &mut GoalStore, rounds: u64, max: u64) {
        let mut projection = create_in(store, "ship SH-088", Some(max)).unwrap();
        projection.rounds_started = rounds;
        store.current = Some(projection);
    }

    #[test]
    fn create_returns_active_revision_one() {
        let mut store = fresh();
        let created = create_in(&mut store, "  ship it  ", None).unwrap();
        assert_eq!(created.goal.revision, 1);
        assert_eq!(created.goal.phase, "active");
        assert_eq!(created.goal.objective, "ship it");
        assert_eq!(created.goal.max_goal_rounds, DEFAULT_MAX_GOAL_ROUNDS);
        assert_eq!(created.rounds_started, 0);
        assert_eq!(store.current.as_ref(), Some(&created));
        assert!(store.seen_goal_ids.contains(&created.goal.id));
    }

    #[test]
    fn create_rejects_second_live_goal_but_allows_after_complete() {
        let mut store = fresh();
        let first = create_in(&mut store, "first", None).unwrap();
        let error = create_in(&mut store, "second", None).unwrap_err();
        assert_eq!(error.code, "GOAL_ALREADY_EXISTS");

        store.current = Some(GoalProjection {
            goal: with_phase(&first.goal, "complete"),
            rounds_started: first.rounds_started,
            created_at: first.created_at,
            updated_at: first.updated_at + 1,
        });
        let second = create_in(&mut store, "second", None).unwrap();
        assert_ne!(second.goal.id, first.goal.id);
        assert_eq!(second.goal.revision, 1);
        assert_eq!(second.goal.phase, "active");
    }

    #[test]
    fn create_rejects_empty_objective_and_bad_round_cap() {
        let mut store = fresh();
        assert_eq!(create_in(&mut store, "   ", None).unwrap_err().code, "GOAL_INVALID_OBJECTIVE");
        assert_eq!(create_in(&mut store, "ok", Some(0)).unwrap_err().code, "GOAL_INVALID_MAX_ROUNDS");
    }

    #[test]
    fn edit_requires_current_revision_and_advances_it() {
        let mut store = fresh();
        let created = create_in(&mut store, "old", Some(10)).unwrap();

        let stale = edit_in(&mut store, &created.goal.id, created.goal.revision + 1, Some("new"), None).unwrap_err();
        assert_eq!(stale.code, "GOAL_STALE_REVISION");

        let missing = edit_in(&mut store, "goal-nope", 1, Some("new"), None).unwrap_err();
        assert_eq!(missing.code, "GOAL_STALE_REVISION");

        let edited = edit_in(&mut store, &created.goal.id, created.goal.revision, Some("  new  "), Some(12)).unwrap();
        assert_eq!(edited.goal.objective, "new");
        assert_eq!(edited.goal.max_goal_rounds, 12);
        assert_eq!(edited.goal.revision, 2);
        assert_eq!(edited.goal.phase, "active");
        assert_eq!(edited.created_at, created.created_at);
        assert!(edited.updated_at >= created.updated_at);
    }

    #[test]
    fn edit_rejects_empty_request_and_empty_objective() {
        let mut store = fresh();
        let created = create_in(&mut store, "old", None).unwrap();
        assert_eq!(
            edit_in(&mut store, &created.goal.id, created.goal.revision, None, None).unwrap_err().code,
            "GOAL_INVALID_EDIT"
        );
        assert_eq!(
            edit_in(&mut store, &created.goal.id, created.goal.revision, Some("  "), None).unwrap_err().code,
            "GOAL_INVALID_OBJECTIVE"
        );
    }

    #[test]
    fn pause_and_resume_follow_phase_rules() {
        let mut store = fresh();
        let created = create_in(&mut store, "cycle", Some(3)).unwrap();

        let paused = transition_in(&mut store, &created.goal.id, created.goal.revision, &["active"], "paused").unwrap();
        assert_eq!(paused.goal.phase, "paused");
        assert_eq!(paused.goal.revision, 2);

        let again = transition_in(&mut store, &paused.goal.id, paused.goal.revision, &["active"], "paused").unwrap_err();
        assert_eq!(again.code, "GOAL_INVALID_TRANSITION");

        let resumed = resume_in(&mut store, &paused.goal.id, paused.goal.revision).unwrap();
        assert_eq!(resumed.goal.phase, "active");
        assert_eq!(resumed.goal.revision, 3);
    }

    #[test]
    fn resume_rejects_exhausted_round_budget() {
        let mut store = fresh();
        seed(&mut store, 3, 3);
        let current = store.current.clone().unwrap();
        let error = resume_in(&mut store, &current.goal.id, current.goal.revision).unwrap_err();
        assert_eq!(error.code, "GOAL_INVALID_TRANSITION");
    }

    #[test]
    fn clear_returns_tombstone_and_allows_recreate() {
        let mut store = fresh();
        let created = create_in(&mut store, "temp", None).unwrap();
        let tombstone = clear_in(&mut store, &created.goal.id, created.goal.revision).unwrap();
        assert_eq!(tombstone.id, created.goal.id);
        assert_eq!(tombstone.revision, created.goal.revision + 1);
        assert!(store.current.is_none());
        assert!(store.seen_goal_ids.contains(&created.goal.id));

        let recreated = create_in(&mut store, "fresh", None).unwrap();
        assert_ne!(recreated.goal.id, created.goal.id);
    }

    #[test]
    fn clear_requires_current_goal_and_matching_revision() {
        let mut store = fresh();
        assert_eq!(clear_in(&mut store, "goal-x", 1).unwrap_err().code, "GOAL_NOT_FOUND");
        let created = create_in(&mut store, "temp", None).unwrap();
        assert_eq!(
            clear_in(&mut store, &created.goal.id, created.goal.revision + 1).unwrap_err().code,
            "GOAL_STALE_REVISION"
        );
    }

    #[test]
    fn store_round_trips_through_json() {
        let mut store = fresh();
        let created = create_in(&mut store, "persist me", Some(9)).unwrap();
        let encoded = serde_json::to_string(&store).unwrap();
        let decoded: GoalStore = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.current, Some(created));
        assert_eq!(decoded.seen_goal_ids.len(), 1);
    }

    #[test]
    fn projection_wire_shape_omits_absent_blocked_reason() {
        let mut store = fresh();
        let created = create_in(&mut store, "shape", None).unwrap();
        let value = serde_json::to_value(&created).unwrap();
        assert_eq!(value["goal"]["id"], created.goal.id);
        assert_eq!(value["goal"]["revision"], 1);
        assert_eq!(value["roundsStarted"], 0);
        assert!(value["goal"].get("blockedReason").is_none());
    }

    #[test]
    fn goal_path_rejects_traversal() {
        let root = Path::new("C:/data");
        assert!(goal_path(root, "../evil").is_err());
        assert!(goal_path(root, "").is_err());
        assert!(goal_path(root, "sess-1").is_ok());
    }
}
