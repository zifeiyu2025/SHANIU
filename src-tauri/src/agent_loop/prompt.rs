//! 系统提示词构造（M1 缺口补齐）。
//!
//! 模型每轮采样前都会收到这段文本（不写入 history，避免重复累积）。
//! 约束：只包含运行上下文（工作区路径/沙箱模式/计划态），**不得**包含密钥、
//! 令牌、会话 rollout 内容等敏感数据（AGENTS.md §3 数据边界）。

use std::path::Path;

use crate::config::SandboxMode;

/// 沙箱模式的中文释义（写入提示词，让模型知道自己的执行边界）。
fn sandbox_clause(mode: SandboxMode) -> &'static str {
    match mode {
        SandboxMode::ReadOnly => "只读：任何写入与命令执行都会被拒绝。",
        SandboxMode::WorkspaceWrite => {
            "工作区写入：可在工作区内读写文件与执行命令；工作区外写入与网络默认被拒绝。"
        }
        SandboxMode::DangerFullAccess => "完全访问：命令不受沙箱限制，请格外谨慎。",
    }
}

/// 构造系统提示词。`plan_mode` 为真时追加只读计划态约束。
pub fn system_instruction(workspace_root: &Path, sandbox: SandboxMode, plan_mode: bool) -> String {
    let workspace = workspace_root.display();
    let mut text = format!(
        "你是 SHANIU，运行在 Windows 桌面的本地编码 Agent。你通过工具直接操作文件与命令来完成用户任务。\n\n\
## 工作区\n\
当前工作区：{workspace}\n\
所有相对路径都相对该目录解析；不要读写工作区之外的路径。\n\
沙箱模式：{}\n\n\
## 工具使用规则\n\
- 动手前先用 read_file / list_dir / search 了解现状，不要凭猜测修改文件。\n\
- 修改文件一律用 apply_patch（结构化补丁）；不要用 unified_exec 重定向写文件。\n\
- 需要运行构建、测试、git 等命令时用 unified_exec，并给出可判读的命令输出。\n\
- 只读工具可以在同一轮里并行调用；写入与执行工具一次只做一件。\n\
- 工具返回错误时先读错误信息再决定下一步；不要重复同样的失败调用。\n\n\
## 审批与安全\n\
- 写入、命令执行等操作可能弹出人工审批；用户拒绝时不要绕过或换个说法重试，改为说明原因并征求指示。\n\
- 破坏性操作（递归删除、强制推送、工作区外写入、上传）会被逐次询问。\n\
- 不要读取或输出任何密钥、令牌、凭据；发现疑似密钥文件时停止并告知用户。\n\n\
## 计划模式\n\
涉及多文件改动或方案不确定时：先调用 enter_plan_mode，仅做只读调研，再用 exit_plan_mode 提交计划等待用户批准；批准后才开始写入。\n\n\
## 回答风格\n\
- 用简体中文，简洁陈述做了什么、结果如何，不堆砌过程。\n\
- 不要编造未实际执行的结果；未验证的内容明确说明。",
        sandbox_clause(sandbox)
    );

    if plan_mode {
        text.push_str(
            "\n\n## 当前状态：计划模式\n\
你正处于计划模式：只有只读工具可用，写入与命令执行会失败。\n\
请完成调研后用 exit_plan_mode 提交完整计划；计划获批前不要尝试修改文件。",
        );
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn includes_workspace_and_sandbox_mode() {
        let text = system_instruction(&PathBuf::from(r"D:\ws\demo"), SandboxMode::WorkspaceWrite, false);
        assert!(text.contains(r"D:\ws\demo"), "必须写明工作区路径");
        assert!(text.contains("工作区写入"), "必须写明沙箱模式");
        // 常驻章节会说明"何时该进计划模式"，但非计划态不得声明已处于计划态
        assert!(!text.contains("当前状态：计划模式"), "非计划态不应出现计划态激活声明");
    }

    #[test]
    fn plan_mode_adds_readonly_clause() {
        let text = system_instruction(&PathBuf::from("D:/ws"), SandboxMode::ReadOnly, true);
        assert!(text.contains("当前状态：计划模式"));
        assert!(text.contains("exit_plan_mode"));
        assert!(text.contains("只读"));
    }

    #[test]
    fn never_mentions_secret_material() {
        // 提示词由运行上下文拼装，不含任何密钥占位（AGENTS.md §3）
        let text = system_instruction(&PathBuf::from("D:/ws"), SandboxMode::DangerFullAccess, false);
        assert!(!text.to_lowercase().contains("api_key"));
        assert!(!text.contains("sk-"));
    }
}
