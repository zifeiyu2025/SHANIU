//! 信任与豁免的持久化（SH-063，docs/02 §3.1.4/§3.1.6）。
//!
//! 三件事必须同时成立：
//! - **批准对象是指纹**：记录里存的是完整 [`FingerprintInput`]，界面能回答「这条授权
//!   是给哪个可执行文件 / argv / cwd / env / 工具清单的」；
//! - **逐工具风险档位由用户指定**：默认 `destructive`，随记录持久化；server 的
//!   `readOnlyHint`/`destructiveHint` 不进入本模块的任何决策路径（ADR-107）；
//! - **变更语义按用户口径（docs/02 §3.1.4(3)）**：工具清单增删、参数 **Schema**
//!   变化 → 重批；描述 / 注解变化 → 只更新摘要，不重批，但**每次连接都重跑注入过滤**
//!   （描述是注入主通道，见 [`super::registry::partition_by_guard`]）。
//!
//! 写盘用临时文件 + 替换；文件损坏或算法版本不认识 → 按空存储处理（fail-closed 为
//! `review`，绝不把损坏当成「已批准」）。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::clock::now_ms;
use crate::events::RiskLevel;

use super::fingerprint::{sha256_hex, FingerprintInput};
use super::guard::Exemption;
use super::manifest::RegistrationPlan;

/// 信任文件（与 `secrets/` 同目录，但**不是**密钥材料）。
pub const TRUST_FILE: &str = "mcp-trust.json";
/// 存储格式版本。换版本 = 旧记录不可信，回到 review。
pub const TRUST_ALGO: &str = "shaniu-mcp-trust-v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustStatus {
    /// 待用户审查：工具不注册、模型看不见。
    Review,
    /// 已批准：按逐工具批准位与风险档位注册。
    Trusted,
    /// 用户阻止：不连接、不注册。
    Blocked,
    /// 可执行文件哈希与批准记录不一致：**不启动、不注册、不列当前清单**，
    /// 等用户显式确认新二进制后重新审查（docs/02 §3.1.4(3)）。
    Retrust,
}

impl TrustStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            TrustStatus::Review => "review",
            TrustStatus::Trusted => "trusted",
            TrustStatus::Blocked => "blocked",
            TrustStatus::Retrust => "retrust",
        }
    }
}

/// 一个工具的批准位 + 用户选定的风险档位。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolGrant {
    pub dispatch_name: String,
    pub raw_name: String,
    /// 用户选定的档位；未批准时也保留（重新勾选时回显上次选择）。
    pub risk: RiskLevel,
    pub approved: bool,
    /// 内容摘要只用于「变了但不用重批」的检测与界面展示，**不进指纹**。
    #[serde(default)]
    pub description_sha256: String,
    #[serde(default)]
    pub schema_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations_sha256: Option<String>,
}

/// 旧清单快照：可执行文件变化时用来展示「上次批准的是什么」。
///
/// **只作参照，不是当前有效清单**——界面必须标注旧版本，且不得据此注册工具。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CachedTool {
    pub dispatch_name: String,
    pub raw_name: String,
    pub description: String,
    pub schema: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<Value>,
}

/// 一次批准记录。`input` 就是被批准的那份身份，界面据此展示「授权给谁」。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrustRecord {
    pub server_id: String,
    /// 由 [`TrustRecord::input`] 计算并固化，避免读盘时重算漂移。
    pub fingerprint: String,
    pub status: TrustStatus,
    /// 最近一次批准时间（ms）；review 记录为 0。
    #[serde(default)]
    pub approved_at_ms: u64,
    pub input: FingerprintInput,
    #[serde(default)]
    pub tools: Vec<ToolGrant>,
    /// 批准时的清单快照（含描述/Schema），仅用于「旧版本参考」展示。
    #[serde(default)]
    pub cached_tools: Vec<CachedTool>,
    /// 注入检测的显式启用记录（A-R4），与信任同生共死。
    #[serde(default)]
    pub exemptions: Vec<Exemption>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl TrustRecord {
    /// 已批准工具及其风险档位（注册表的唯一输入）。
    pub fn approved_tools(&self) -> Vec<(String, RiskLevel)> {
        self.tools
            .iter()
            .filter(|grant| grant.approved)
            .map(|grant| (grant.dispatch_name.clone(), grant.risk))
            .collect()
    }

    pub fn has_approved_tools(&self) -> bool {
        self.tools.iter().any(|grant| grant.approved)
    }
}

/// 用户在审查界面上对单个工具的裁决。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolDecision {
    pub dispatch_name: String,
    pub approved: bool,
    pub risk: RiskLevel,
}

impl ToolDecision {
    pub fn approve(dispatch_name: impl Into<String>, risk: RiskLevel) -> Self {
        Self { dispatch_name: dispatch_name.into(), approved: true, risk: sanitize_user_risk(risk) }
    }

    pub fn skip(dispatch_name: impl Into<String>) -> Self {
        Self { dispatch_name: dispatch_name.into(), approved: false, risk: RiskLevel::Destructive }
    }
}

/// 用户可选的档位里没有 `plan_review`（那是一次计划评审，不是工具属性）。
/// 传进来一律落到 `destructive`——只紧不松。
pub fn sanitize_user_risk(risk: RiskLevel) -> RiskLevel {
    match risk {
        RiskLevel::PlanReview => RiskLevel::Destructive,
        other => other,
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TrustError {
    #[error("服务器 {0} 没有待审查记录；请先连接并完成首连审查")]
    NoPendingReview(String),
    #[error("指纹不匹配（记录 {recorded}，提交 {submitted}）：清单或身份已变化，请重新连接审查")]
    FingerprintMismatch { recorded: String, submitted: String },
    #[error("信任记录写入失败: {0}")]
    Persist(String),
}

/// 清单变化（§3.1.4(3) 的 diff 语义）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolChangeKind {
    Added,
    Removed,
    /// 只有描述 / 注解变化：不重批，但下次连接仍必须重跑注入过滤。
    ContentChanged,
    /// 参数 Schema 变化：用户批准时看到的工具形态变了，必须重批。
    SchemaChanged,
}

impl ToolChangeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ToolChangeKind::Added => "added",
            ToolChangeKind::Removed => "removed",
            ToolChangeKind::ContentChanged => "content_changed",
            ToolChangeKind::SchemaChanged => "schema_changed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolChange {
    pub dispatch_name: String,
    pub kind: ToolChangeKind,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ManifestDiff {
    pub changes: Vec<ToolChange>,
}

impl ManifestDiff {
    /// **增删与 Schema 变化要重批**；描述 / 注解变化只更新摘要（用户口径）。
    ///
    /// 原因：描述是注入主通道但不构成授权面（风险档位由用户设定，描述被改也不会给工具
    /// 增加权限），只要每次连接都重跑注入过滤即可；注解只做展示，不参与裁决。
    /// 而 Schema 是参数契约——用户批准时看到的工具形态变了，不能悄悄过。
    pub fn requires_review(&self) -> bool {
        self.changes.iter().any(|change| {
            matches!(
                change.kind,
                ToolChangeKind::Added | ToolChangeKind::Removed | ToolChangeKind::SchemaChanged
            )
        })
    }

    pub fn content_changed(&self) -> Vec<String> {
        self.changes
            .iter()
            .filter(|change| change.kind == ToolChangeKind::ContentChanged)
            .map(|change| change.dispatch_name.clone())
            .collect()
    }

    pub fn schema_changed(&self) -> Vec<String> {
        self.changes
            .iter()
            .filter(|change| change.kind == ToolChangeKind::SchemaChanged)
            .map(|change| change.dispatch_name.clone())
            .collect()
    }

    pub fn explain(&self) -> String {
        if self.changes.is_empty() {
            return "工具清单没有变化".to_string();
        }
        self.changes
            .iter()
            .map(|change| {
                let label = match change.kind {
                    ToolChangeKind::Added => "新增",
                    ToolChangeKind::Removed => "删除",
                    ToolChangeKind::ContentChanged => "描述/注解变化（不重批）",
                    ToolChangeKind::SchemaChanged => "参数 Schema 变化（必须重批）",
                };
                format!("{label}: {}", change.dispatch_name)
            })
            .collect::<Vec<_>>()
            .join("；")
    }
}

/// 比对旧批准清单与新清单。
pub fn diff_manifest(old: &[ToolGrant], new: &[ToolGrant]) -> ManifestDiff {
    let old_map: BTreeMap<&str, &ToolGrant> =
        old.iter().map(|grant| (grant.dispatch_name.as_str(), grant)).collect();
    let new_map: BTreeMap<&str, &ToolGrant> =
        new.iter().map(|grant| (grant.dispatch_name.as_str(), grant)).collect();

    let mut changes = Vec::new();
    for (name, grant) in &new_map {
        match old_map.get(name) {
            None => changes.push(ToolChange {
                dispatch_name: (*name).to_string(),
                kind: ToolChangeKind::Added,
            }),
            Some(previous) => {
                if previous.schema_sha256 != grant.schema_sha256 {
                    changes.push(ToolChange {
                        dispatch_name: (*name).to_string(),
                        kind: ToolChangeKind::SchemaChanged,
                    });
                } else if previous.description_sha256 != grant.description_sha256
                    || previous.annotations_sha256 != grant.annotations_sha256
                {
                    changes.push(ToolChange {
                        dispatch_name: (*name).to_string(),
                        kind: ToolChangeKind::ContentChanged,
                    });
                }
            }
        }
    }
    for name in old_map.keys() {
        if !new_map.contains_key(name) {
            changes.push(ToolChange {
                dispatch_name: (*name).to_string(),
                kind: ToolChangeKind::Removed,
            });
        }
    }
    changes.sort_by(|a, b| a.dispatch_name.cmp(&b.dispatch_name));
    ManifestDiff { changes }
}

/// 把清单转换结果转成「旧版本参考」快照。
pub fn cached_from_plan(plan: &RegistrationPlan) -> Vec<CachedTool> {
    plan.accepted
        .iter()
        .map(|tool| CachedTool {
            dispatch_name: tool.dispatch_name.clone(),
            raw_name: tool.raw_name.clone(),
            description: tool.spec.description.clone(),
            schema: tool.spec.parameters.clone(),
            annotations: tool.annotations.clone(),
        })
        .collect()
}

/// 把清单转换结果转成批准项（默认 `destructive`、默认不勾选）。
pub fn grants_from_plan(plan: &RegistrationPlan) -> Vec<ToolGrant> {
    plan.accepted
        .iter()
        .map(|tool| ToolGrant {
            dispatch_name: tool.dispatch_name.clone(),
            raw_name: tool.raw_name.clone(),
            risk: RiskLevel::Destructive,
            approved: false,
            description_sha256: description_digest(&tool.spec.description),
            schema_sha256: schema_digest(&tool.spec.parameters),
            annotations_sha256: annotations_digest(tool.annotations.as_ref()),
        })
        .collect()
}

/// 内容变化只更新摘要（不重批，但下次仍能检测到再次变化）。
pub fn refresh_digests(record: &mut TrustRecord, fresh: &[ToolGrant]) {
    let fresh_map: BTreeMap<&str, &ToolGrant> =
        fresh.iter().map(|grant| (grant.dispatch_name.as_str(), grant)).collect();
    for grant in &mut record.tools {
        if let Some(latest) = fresh_map.get(grant.dispatch_name.as_str()) {
            grant.description_sha256 = latest.description_sha256.clone();
            grant.schema_sha256 = latest.schema_sha256.clone();
            grant.annotations_sha256 = latest.annotations_sha256.clone();
        }
    }
}

pub fn description_digest(normalized: &str) -> String {
    sha256_hex(normalized.as_bytes())
}

pub fn schema_digest(schema: &Value) -> String {
    sha256_hex(schema.to_string().as_bytes())
}

pub fn annotations_digest(annotations: Option<&Value>) -> Option<String> {
    annotations.map(|value| sha256_hex(value.to_string().as_bytes()))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TrustDocument {
    algo: String,
    #[serde(default)]
    servers: Vec<TrustRecord>,
}

/// 持久化信任存储。所有写操作立即落盘；损坏文件按空存储处理。
#[derive(Debug, Clone)]
pub struct TrustStore {
    path: PathBuf,
    records: BTreeMap<String, TrustRecord>,
}

impl TrustStore {
    pub fn load(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let mut store = Self { path, records: BTreeMap::new() };
        match std::fs::read_to_string(&store.path) {
            Ok(raw) => match serde_json::from_str::<TrustDocument>(&raw) {
                Ok(document) if document.algo == TRUST_ALGO => {
                    for record in document.servers {
                        store.records.insert(record.server_id.clone(), record);
                    }
                }
                Ok(document) => tracing::warn!(
                    "MCP 信任记录版本不认识（{}，期望 {}），按空存储处理",
                    document.algo,
                    TRUST_ALGO
                ),
                Err(error) => tracing::warn!(
                    "MCP 信任记录损坏（{}），按空存储处理（fail-closed 为 review）: {error}",
                    store.path.display()
                ),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => tracing::warn!(
                "MCP 信任记录读取失败（{}），按空存储处理: {error}",
                store.path.display()
            ),
        }
        store
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn record(&self, server_id: &str) -> Option<&TrustRecord> {
        self.records.get(server_id)
    }

    pub fn records(&self) -> Vec<TrustRecord> {
        self.records.values().cloned().collect()
    }

    /// 直接写入一条记录并落盘（运行时在指纹未变时刷新摘要后调用）。
    pub fn upsert(&mut self, record: TrustRecord) -> Result<(), TrustError> {
        self.records.insert(record.server_id.clone(), record);
        self.save()
    }

    /// 写一条待审查记录：状态置 `review`、批准时间清零、工具全部未勾选。
    ///
    /// 指纹变化时旧豁免不继承（旧授权不属于新身份）；指纹不变的重连保留豁免。
    pub fn store_review(
        &mut self,
        server_id: &str,
        input: FingerprintInput,
        tools: Vec<ToolGrant>,
        cached_tools: Vec<CachedTool>,
        reason: Option<String>,
    ) -> Result<TrustRecord, TrustError> {
        let fingerprint = input.fingerprint();
        let exemptions = match self.records.get(server_id) {
            Some(record) if record.fingerprint == fingerprint => record.exemptions.clone(),
            _ => Vec::new(),
        };
        let record = TrustRecord {
            server_id: server_id.to_string(),
            fingerprint,
            status: TrustStatus::Review,
            approved_at_ms: 0,
            input,
            tools,
            cached_tools,
            exemptions,
            reason,
        };
        self.records.insert(server_id.to_string(), record.clone());
        self.save()?;
        Ok(record)
    }

    /// 应用用户在审查界面的裁决。
    pub fn resolve(
        &mut self,
        server_id: &str,
        submitted_fingerprint: &str,
        decisions: &[ToolDecision],
    ) -> Result<TrustRecord, TrustError> {
        let record = self
            .records
            .get_mut(server_id)
            .ok_or_else(|| TrustError::NoPendingReview(server_id.to_string()))?;
        if record.fingerprint != submitted_fingerprint {
            return Err(TrustError::FingerprintMismatch {
                recorded: record.fingerprint.clone(),
                submitted: submitted_fingerprint.to_string(),
            });
        }
        let decision_map: BTreeMap<&str, &ToolDecision> = decisions
            .iter()
            .map(|decision| (decision.dispatch_name.as_str(), decision))
            .collect();
        for grant in &mut record.tools {
            match decision_map.get(grant.dispatch_name.as_str()) {
                Some(decision) if decision.approved => {
                    grant.approved = true;
                    grant.risk = sanitize_user_risk(decision.risk);
                }
                _ => {
                    grant.approved = false;
                    grant.risk = RiskLevel::Destructive;
                }
            }
        }
        record.status = if record.has_approved_tools() {
            TrustStatus::Trusted
        } else {
            TrustStatus::Blocked
        };
        record.approved_at_ms = now_ms();
        let snapshot = record.clone();
        self.save()?;
        Ok(snapshot)
    }

    /// 撤销：记录整体移除，下次连接回到 review；豁免随记录一起消失。
    pub fn revoke(&mut self, server_id: &str) -> Result<Option<TrustRecord>, TrustError> {
        let removed = self.records.remove(server_id);
        if removed.is_some() {
            self.save()?;
        }
        Ok(removed)
    }

    /// 把某服务器的豁免（显式启用注入工具）写回记录并落盘。
    pub fn set_exemptions(
        &mut self,
        server_id: &str,
        exemptions: Vec<Exemption>,
    ) -> Result<(), TrustError> {
        let Some(record) = self.records.get_mut(server_id) else {
            return Err(TrustError::NoPendingReview(server_id.to_string()));
        };
        record.exemptions = exemptions;
        self.save()
    }

    /// 全部持久化豁免（启动时灌回 [`super::guard::ExemptionStore`]）。
    pub fn exemptions(&self) -> Vec<Exemption> {
        self.records
            .values()
            .flat_map(|record| record.exemptions.iter().cloned())
            .collect()
    }

    fn save(&self) -> Result<(), TrustError> {
        let document = TrustDocument {
            algo: TRUST_ALGO.to_string(),
            servers: self.records.values().cloned().collect(),
        };
        let raw = serde_json::to_string_pretty(&document)
            .map_err(|error| TrustError::Persist(error.to_string()))?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| TrustError::Persist(error.to_string()))?;
        }
        let tmp = crate::config::temp_sibling(&self.path);
        std::fs::write(&tmp, raw).map_err(|error| TrustError::Persist(error.to_string()))?;
        replace_file(&tmp, &self.path).map_err(|error| TrustError::Persist(error.to_string()))
    }
}

/// 临时文件替换目标文件。Windows 上 `rename` 遇到已存在的目标会失败，
/// 因此先删后换——窗口极小，且文件损坏的兜底是 fail-closed 为 review。
fn replace_file(tmp: &Path, target: &Path) -> std::io::Result<()> {
    match std::fs::rename(tmp, target) {
        Ok(()) => Ok(()),
        Err(_) if target.exists() => {
            std::fs::remove_file(target)?;
            std::fs::rename(tmp, target)
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_client::fingerprint::FingerprintInput;
    use crate::mcp_client::guard::RuleId;

    fn temp_dir(tag: &str) -> crate::test_support::TestTempDir {
        crate::test_support::TestTempDir::new(&format!("trust-{tag}"))
    }

    fn input(env: BTreeMap<String, String>, tools: &[&str]) -> FingerprintInput {
        let tools: Vec<String> = tools.iter().map(|t| t.to_string()).collect();
        FingerprintInput::new(
            "stdio",
            "2024-11-05",
            r"C:\tools\server.exe",
            &"a".repeat(64),
            &["--serve".to_string()],
            r"C:\workspace",
            &env,
            "srv-name",
            "1.0.0",
            &tools,
            None,
        )
    }

    fn grants() -> Vec<ToolGrant> {
        vec![
            ToolGrant {
                dispatch_name: "mcp__srv__read".into(),
                raw_name: "read".into(),
                risk: RiskLevel::Destructive,
                approved: false,
                description_sha256: "d1".into(),
                schema_sha256: "s1".into(),
                annotations_sha256: None,
            },
            ToolGrant {
                dispatch_name: "mcp__srv__write".into(),
                raw_name: "write".into(),
                risk: RiskLevel::Destructive,
                approved: false,
                description_sha256: "d2".into(),
                schema_sha256: "s2".into(),
                annotations_sha256: None,
            },
        ]
    }

    fn review_record(store_path: &Path, server_id: &str) -> TrustRecord {
        let input = input(BTreeMap::new(), &["mcp__srv__read", "mcp__srv__write"]);
        let mut store = TrustStore::load(store_path);
        store.store_review(server_id, input, grants(), Vec::new(), None).unwrap()
    }

    #[test]
    fn review_is_default_and_round_trips_through_disk() {
        let dir = temp_dir("roundtrip");
        let path = dir.join(TRUST_FILE);
        let record = review_record(&path, "srv");
        assert_eq!(record.status, TrustStatus::Review);
        assert!(record.approved_tools().is_empty(), "未批准时没有可用工具");

        let reloaded = TrustStore::load(&path);
        let again = reloaded.record("srv").expect("记录必须落盘");
        assert_eq!(again.fingerprint, record.fingerprint);
        assert_eq!(again.input.executable, record.input.executable);
    }

    #[test]
    fn corrupt_store_fails_closed_to_empty_review() {
        let dir = temp_dir("corrupt");
        let path = dir.join(TRUST_FILE);
        std::fs::write(&path, "{not json").unwrap();
        let store = TrustStore::load(&path);
        assert!(store.record("srv").is_none(), "损坏文件不得被当成已批准");
    }

    #[test]
    fn resolve_persists_per_tool_risk_and_only_approved_tools() {
        let dir = temp_dir("resolve");
        let path = dir.join(TRUST_FILE);
        let record = review_record(&path, "srv");
        let mut store = TrustStore::load(&path);
        let resolved = store
            .resolve(
                "srv",
                &record.fingerprint,
                &[
                    ToolDecision::approve("mcp__srv__read", RiskLevel::Read),
                    ToolDecision::skip("mcp__srv__write"),
                ],
            )
            .unwrap();
        assert_eq!(resolved.status, TrustStatus::Trusted);
        assert_eq!(resolved.approved_tools(), vec![("mcp__srv__read".to_string(), RiskLevel::Read)]);

        // 重新加载：用户的档位必须持久化，未批准工具不得复活
        let reloaded = TrustStore::load(&path).record("srv").cloned().unwrap();
        assert_eq!(reloaded.approved_tools(), resolved.approved_tools());
        assert!(!reloaded.tools.iter().find(|g| g.dispatch_name.ends_with("write")).unwrap().approved);
    }

    #[test]
    fn approving_nothing_blocks_the_server() {
        let dir = temp_dir("block");
        let path = dir.join(TRUST_FILE);
        let record = review_record(&path, "srv");
        let mut store = TrustStore::load(&path);
        let resolved = store.resolve("srv", &record.fingerprint, &[]).unwrap();
        assert_eq!(resolved.status, TrustStatus::Blocked);
        assert!(resolved.approved_tools().is_empty());
    }

    #[test]
    fn fingerprint_mismatch_is_rejected() {
        let dir = temp_dir("mismatch");
        let path = dir.join(TRUST_FILE);
        review_record(&path, "srv");
        let mut store = TrustStore::load(&path);
        let err = store
            .resolve("srv", "deadbeef", &[ToolDecision::approve("mcp__srv__read", RiskLevel::Read)])
            .unwrap_err();
        assert!(matches!(err, TrustError::FingerprintMismatch { .. }));
        assert_eq!(store.record("srv").unwrap().status, TrustStatus::Review);
    }

    #[test]
    fn plan_review_is_clamped_to_destructive() {
        let dir = temp_dir("clamp");
        let path = dir.join(TRUST_FILE);
        let record = review_record(&path, "srv");
        let mut store = TrustStore::load(&path);
        let resolved = store
            .resolve(
                "srv",
                &record.fingerprint,
                &[ToolDecision::approve("mcp__srv__read", RiskLevel::PlanReview)],
            )
            .unwrap();
        assert_eq!(resolved.approved_tools()[0].1, RiskLevel::Destructive);
    }

    #[test]
    fn revoke_removes_record_and_exemptions() {
        let dir = temp_dir("revoke");
        let path = dir.join(TRUST_FILE);
        let record = review_record(&path, "srv");
        let mut store = TrustStore::load(&path);
        store
            .set_exemptions(
                "srv",
                vec![Exemption {
                    server_id: "srv".into(),
                    fingerprint: record.fingerprint.clone(),
                    dispatch_name: "mcp__srv__read".into(),
                    rule: RuleId::InstructionOverride,
                    hit_fragment: "ignore previous instructions".into(),
                    granted_at_ms: 1,
                }],
            )
            .unwrap();
        assert_eq!(store.exemptions().len(), 1);

        assert!(store.revoke("srv").unwrap().is_some());
        assert!(store.record("srv").is_none());
        assert!(store.exemptions().is_empty(), "撤销信任必须连豁免一起撤销");
        assert!(TrustStore::load(&path).record("srv").is_none());
    }

    #[test]
    fn env_values_are_never_written_in_plaintext() {
        let dir = temp_dir("env");
        let path = dir.join(TRUST_FILE);
        let secret = "plaintext-token-xyz";
        let mut env = BTreeMap::new();
        env.insert("API_TOKEN".to_string(), secret.to_string());
        let mut store = TrustStore::load(&path);
        store
            .store_review("srv", input(env, &["mcp__srv__read"]), grants(), Vec::new(), None)
            .unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains(secret), "env 明文不得落盘");
        assert!(raw.contains("API_TOKEN"), "键名要保留供界面展示");
    }

    #[test]
    fn manifest_diff_review_semantics_follow_user_ruling() {
        // ① 描述变更：不重批（注入过滤在每次连接时重跑，不靠这里）。
        let old = grants();
        let mut description_only = old.clone();
        description_only[0].description_sha256 = "changed".into();
        let diff = diff_manifest(&old, &description_only);
        assert!(!diff.requires_review(), "改描述不重批（用户口径）");
        assert_eq!(diff.content_changed(), vec!["mcp__srv__read".to_string()]);
        assert!(diff.explain().contains("不重批"));

        // ② 注解变更：不重批（只做展示，从不参与裁决）。
        let mut annotations_only = old.clone();
        annotations_only[0].annotations_sha256 = Some("annotated".into());
        let diff = diff_manifest(&old, &annotations_only);
        assert!(!diff.requires_review(), "改注解不重批");
        assert_eq!(diff.content_changed(), vec!["mcp__srv__read".to_string()]);

        // ③ Schema 变更：必须重批（参数契约变了，用户看到的工具形态也变了）。
        let mut schema_only = old.clone();
        schema_only[0].schema_sha256 = "schema-v2".into();
        let diff = diff_manifest(&old, &schema_only);
        assert!(diff.requires_review(), "改 Schema 必须重批");
        assert_eq!(diff.schema_changed(), vec!["mcp__srv__read".to_string()]);
        assert!(diff.explain().contains("Schema"));

        // ④ 增删：必须重批。
        let mut added = old.clone();
        added.push(ToolGrant {
            dispatch_name: "mcp__srv__exec".into(),
            raw_name: "exec".into(),
            risk: RiskLevel::Destructive,
            approved: false,
            description_sha256: "d3".into(),
            schema_sha256: "s3".into(),
            annotations_sha256: None,
        });
        assert!(diff_manifest(&old, &added).requires_review(), "新增工具必须重批");

        let removed: Vec<ToolGrant> = old.iter().skip(1).cloned().collect();
        assert!(diff_manifest(&old, &removed).requires_review(), "删除工具必须重批");
    }

    #[test]
    fn refresh_digests_keeps_approval_but_updates_content_baseline() {
        let mut record = TrustRecord {
            server_id: "srv".into(),
            fingerprint: "fp".into(),
            status: TrustStatus::Trusted,
            approved_at_ms: 1,
            input: input(BTreeMap::new(), &["mcp__srv__read"]),
            tools: vec![ToolGrant {
                dispatch_name: "mcp__srv__read".into(),
                raw_name: "read".into(),
                risk: RiskLevel::Read,
                approved: true,
                description_sha256: "old".into(),
                schema_sha256: "schema".into(),
                annotations_sha256: None,
            }],
            cached_tools: Vec::new(),
            exemptions: Vec::new(),
            reason: None,
        };
        let fresh = vec![ToolGrant {
            dispatch_name: "mcp__srv__read".into(),
            raw_name: "read".into(),
            risk: RiskLevel::Destructive,
            approved: false,
            description_sha256: "new".into(),
            schema_sha256: "schema".into(),
            annotations_sha256: None,
        }];
        let diff = diff_manifest(&record.tools, &fresh);
        assert!(!diff.requires_review());
        refresh_digests(&mut record, &fresh);
        assert!(record.tools[0].approved, "改描述不得撤销用户批准");
        assert_eq!(record.tools[0].risk, RiskLevel::Read, "改描述不得改动用户档位");
        assert_eq!(record.tools[0].description_sha256, "new");
    }
}
