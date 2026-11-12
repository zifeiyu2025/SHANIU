//! 权限预设：把 `(sandbox, approval)` 两维合成**一个 id** 的唯一事实源。
//!
//! 为什么需要这张表：宿主的安全面是两维（沙箱边界 × 审批策略），但用户选的是
//! 「我现在处于哪种安全姿态」。两维分开写会出现「沙箱放宽了、审批还是逐次问」
//! 这种既不像只读也不像自主的中间态——2026-11-11 的自主档故障就是这么来的
//! （选了 never 但沙箱仍是只读，工具全部被拒）。预设把两维绑成一组语义，
//! 设置页、`/permission`、会话档案、resume 全部只认 id。
//!
//! 档位集合逐字对齐 DSH（对照表见 `align/permission-presets.md`）：
//! `read-only` / `workspace-write` / `danger-full-access` 三档，`(sandbox, approval)`
//! 组合与 DSH 相同。**没有扩展档**：2026-11-11 曾加过 `workspace-write-auto`
//! （`workspace_write` + `never`），2026-11-12 用户裁定「权限档位对齐 dsh 就行，
//! 不要搞复杂」后移除——想不被打断就选完全权限，那是 DSH 的答案。
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
}

/// 预设全集，顺序即界面呈现顺序（权限递增）。
pub const PRESETS: [Preset; 3] = [
    Preset { id: "read-only", sandbox: SandboxMode::ReadOnly, approval: ApprovalMode::OnRequest },
    Preset {
        id: "workspace-write",
        sandbox: SandboxMode::WorkspaceWrite,
        approval: ApprovalMode::OnRequest,
    },
    Preset {
        id: "danger-full-access",
        sandbox: SandboxMode::DangerFullAccess,
        approval: ApprovalMode::Never,
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
        Self { id: CUSTOM, sandbox, approval }
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
    }

    /// 2026-11-12 用户裁定：档位集合对齐 DSH，自造的 `workspace-write-auto` 移除。
    /// 任何把它加回表里的改动都必须在这里变红——「扩展档」不是可以悄悄长回来的东西。
    #[test]
    fn removed_extension_preset_is_not_selectable() {
        assert_eq!(PRESETS.len(), 3, "预设只剩 DSH 的三档");
        assert_eq!(from_id("workspace-write-auto"), None, "扩展档不得重新出现在表里");
        assert!(PRESETS.iter().all(|preset| preset.id != "workspace-write-auto"));
        // `(workspace_write, never)` 现在是「自定义」：想不被打断就选 danger-full-access
        assert_eq!(resolve(SandboxMode::WorkspaceWrite, ApprovalMode::Never), None);
        assert_eq!(id_of(SandboxMode::WorkspaceWrite, ApprovalMode::Never), CUSTOM);
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
    fn three_presets_keep_dsh_order() {
        assert_eq!(
            PRESETS.iter().map(|preset| preset.id).collect::<Vec<_>>(),
            vec!["read-only", "workspace-write", "danger-full-access"],
            "目录顺序即界面顺序；多一档少一档都必须在这里显形"
        );
    }

    /// 非标准组合一律 `custom`，绝不回落成具体预设。
    #[test]
    fn non_standard_combinations_stay_custom() {
        for (sandbox, approval) in [
            (SandboxMode::WorkspaceWrite, ApprovalMode::Never),
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
