import type { ApprovalMode, SandboxMode } from '../transport/types';

/**
 * 安全档位的展示元数据（SH-040 引入，Composer 的权限席位与设置界面共用）。
 *
 * 收成一处而不是两边各写一份，是因为这两处显示的是**同一个后端状态**
 * （`config.sandbox.mode`）。两份字面量迟早会漂成两套名字，
 * 而用户看到"输入卡说完全权限、设置里说工作区可写"时无法判断哪个是真的。
 *
 * 命名分工照 codex：机器名（`danger_full_access`）留在协议与配置文件里吓人，
 * label 用中性词，后果由 hint 承担。
 */
export const SANDBOX_OPTIONS: {
  value: SandboxMode;
  label: string;
  hint: string;
  danger?: boolean;
}[] = [
  { value: 'read_only', label: '只读', hint: 'Agent 只能读工作区文件，写入与命令都要审批。' },
  { value: 'workspace_write', label: '工作区可写', hint: '可改工作区内的文件并跑命令，越出工作区仍需审批。' },
  {
    value: 'danger_full_access',
    label: '完全访问',
    hint: '可不经审批修改本机任意文件、命令可访问网络。',
    danger: true,
  },
];

export const APPROVAL_OPTIONS: { value: ApprovalMode; label: string; hint: string }[] = [
  { value: 'ask', label: '逐次询问', hint: '写入与命令每次都问。' },
  { value: 'auto_read_only', label: '只读自动放行', hint: '只读工具自动通过，其余仍逐次询问。' },
];

/** 取档位标签；未知值退回线名本身，而不是编一个"默认"出来骗人。 */
export function sandboxLabel(mode: SandboxMode): string {
  return SANDBOX_OPTIONS.find((option) => option.value === mode)?.label ?? mode;
}

export function approvalLabel(mode: ApprovalMode): string {
  return APPROVAL_OPTIONS.find((option) => option.value === mode)?.label ?? mode;
}
