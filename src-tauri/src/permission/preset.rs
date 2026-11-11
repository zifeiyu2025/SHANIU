//! 权限预设：把 `(sandbox, approval)` 两维合成**一个 id** 的唯一事实源。
//!
//! 为什么需要这张表：宿主的安全面是两维（沙箱边界 × 审批策略），但用户选的是
//! 「我现在处于哪种安全姿态」。两维分开写会出现「沙箱放宽了、审批还是逐次问」
//! 这种既不像只读也不像自主的中间态——2026-11-11 的自主档故障就是这么来的
//! （选了 never 但沙箱仍是只读，工具全部被拒）。预设把两维绑成一组语义，
//! 设置页、`/permission`、会话档案、resume 全部只认 id。
//!
//! 档位语义以 DSH 为准（对照表见 `align/permission-presets.md`）：
//! - `read-only` / `workspace-write` / `danger-full-access` 逐字采用 DSH 的
//!   `(sandbox, approval)` 组合；
//! - `workspace-write-auto` 是本仓库扩展（DSH 无此档）：区内写入不问、越界由
//!   沙箱拒绝，用于无人值守。**`extension` 只在内部标记，界面不得显示「扩展」字样**。
//!
//! 不在表里的组合（`on_failure`、`full_access`、任何混搭）一律派生为
//! [`CUSTOM`]：如实显示「自定义」，**不回落成某个具体预设**——把 `full_access`
//! 显示成 `danger-full-access` 会让用户以为自己在 `never` 档。

use crate::config::{ApprovalMode, SandboxMode};

/// 派生档：不是预设，不进 [`PRESETS`]，也不可被 `from_id` 选中。
///
/// 存在的唯一理由是让读侧如实表达「当前组合不在预设表里」，而不是猜一个最像的。
pub const CUSTOM: &str = "custom";

/// 一档预设：id + 它绑定的两维。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preset {
    pub id: &'static str,
    pub sandbox: SandboxMode,
    pub approval: ApprovalMode,
    /// 本仓库扩展档（DSH 没有）。仅内部标记，界面不得展示。
    pub extension: bool,
}

/// 预设全集，顺序即界面呈现顺序（权限递增）。
pub const PRESETS: [Preset; 4] = [
    Preset {
        id: "read-only",
        sandbox: SandboxMode::ReadOnly,
        approval: ApprovalMode::OnRequest,
        extension: false,
    },
    Preset {
        id: "workspace-write",
        sandbox: SandboxMode::WorkspaceWrite,
        approval: ApprovalMode::OnRequest,
        extension: false,
    },
    Preset {
        id: "danger-full-access",
        sandbox: SandboxMode::DangerFullAccess,
        approval: ApprovalMode::Never,
        extension: false,
    },
    Preset {
        id: "workspace-write-auto",
        sandbox: SandboxMode::WorkspaceWrite,
        approval: ApprovalMode::Never,
        extension: true,
    },
];

/// 按 id 取预设；未知 id 返回 None（调用方报错，不回落）。
pub fn from_id(id: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|preset| preset.id == id)
}

/// `(sandbox, approval)` → 预设；**精确匹配**，不匹配返回 None（派生为 [`CUSTOM`]）。
pub fn resolve(sandbox: SandboxMode, approval: ApprovalMode) -> Option<&'static Preset> {
    PRESETS
        .iter()
        .find(|preset| preset.sandbox == sandbox && preset.approval == approval)
}

impl Preset {
    /// 由任意两维合成一个「自定义」值：只用于把**旧档案的沙箱档 + 当前审批档**
    /// 带进 [`crate::app_state::AppState::reset_policy`] 的单一入口。
    ///
    /// 它不是预设：`id` 恒为 [`CUSTOM`]，`from_id` 永远选不中它，界面显示「自定义」。
    pub fn custom(sandbox: SandboxMode, approval: ApprovalMode) -> Self {
        Self { id: CUSTOM, sandbox, approval, extension: false }
    }
}

/// `(sandbox, approval)` → 读侧展示值：命中预设给 id，否则 [`CUSTOM`]。
pub fn id_of(sandbox: SandboxMode, approval: ApprovalMode) -> &'static str {
    resolve(sandbox, approval).map_or(CUSTOM, |preset| preset.id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_and_cover_the_table() {
        let mut ids: Vec<&str> = PRESETS.iter().map(|preset| preset.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), PRESETS.len(), "预设 id 重复");
        assert_eq!(
            PRESETS.iter().filter(|preset| preset.extension).map(|preset| preset.id).collect::<Vec<_>>(),
            vec!["workspace-write-auto"],
            "扩展档只有一档，且仅内部标记"
        );
    }

    #[test]
    fn three_shipped_presets_match_dsh_bundles() {
        // 逐字对齐 DSH：read-only × ask、workspace-write × ask、danger × never
        assert_eq!(
            from_id("read-only").map(|preset| (preset.sandbox, preset.approval)),
            Some((SandboxMode::ReadOnly, ApprovalMode::OnRequest))
        );
        assert_eq!(
            from_id("workspace-write").map(|preset| (preset.sandbox, preset.approval)),
            Some((SandboxMode::WorkspaceWrite, ApprovalMode::OnRequest))
        );
        assert_eq!(
            from_id("danger-full-access").map(|preset| (preset.sandbox, preset.approval)),
            Some((SandboxMode::DangerFullAccess, ApprovalMode::Never))
        );
    }

    #[test]
    fn extension_preset_binds_never_to_workspace_write() {
        let preset = from_id("workspace-write-auto").expect("扩展档必须在表里");
        assert_eq!(preset.sandbox, SandboxMode::WorkspaceWrite);
        assert_eq!(preset.approval, ApprovalMode::Never);
        assert!(preset.extension);
    }

    /// 非标准组合一律 `custom`，绝不回落成具体预设。
    #[test]
    fn non_standard_combinations_stay_custom() {
        for (sandbox, approval) in [
            (SandboxMode::WorkspaceWrite, ApprovalMode::OnFailure),
            (SandboxMode::WorkspaceWrite, ApprovalMode::FullAccess),
            (SandboxMode::ReadOnly, ApprovalMode::Never),
            (SandboxMode::DangerFullAccess, ApprovalMode::OnRequest),
            (SandboxMode::DangerFullAccess, ApprovalMode::FullAccess),
        ] {
            assert_eq!(resolve(sandbox, approval), None, "{sandbox:?} × {approval:?} 不得命中预设");
            assert_eq!(id_of(sandbox, approval), CUSTOM);
        }
        // custom 不是可选项：既不能按 id 选中，也不在表里
        assert_eq!(from_id(CUSTOM), None);
        assert!(PRESETS.iter().all(|preset| preset.id != CUSTOM));
    }

    #[test]
    fn every_table_entry_round_trips_through_id() {
        for preset in PRESETS {
            assert_eq!(from_id(preset.id), Some(&preset));
            assert_eq!(id_of(preset.sandbox, preset.approval), preset.id);
        }
    }
}
