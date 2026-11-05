import type { Draft } from 'immer';
import { newToolCallNode } from '../../nodes/builders';
import type { ToolCallNode, ToolCallStatus } from '../../nodes/types';
import type { EventMsg, ToolCallResult } from '../../transport/types';
import type { SessionState } from '../session-store';

type ToolCallEvent = Extract<
  EventMsg,
  { type: 'tool_call_begin' | 'tool_call_output_delta' | 'tool_call_end' }
>;

/** 权威结果 → 节点状态（穷尽映射：协议新增变体时编译期报警）。 */
const STATUS_FROM_RESULT: Record<ToolCallResult, ToolCallStatus> = {
  ok: 'ok',
  error: 'error',
  cancelled: 'cancelled',
  denied: 'denied',
};

/** 从尾部反查：同一轮里 callId 唯一，且 delta/end 紧跟其 begin 之后。 */
function findByCallId(state: Draft<SessionState>, callId: string): Draft<ToolCallNode> | undefined {
  for (let i = state.nodes.length - 1; i >= 0; i -= 1) {
    const node = state.nodes[i];
    if (node.kind === 'tool_call' && node.callId === callId) return node;
  }
  return undefined;
}

/**
 * 工具调用三段事件归约：begin 建节点 → output_delta 按通道追加 → end 收敛状态。
 * delta/end 找不到对应节点时静默丢弃：会话 reset、事件乱序（如 end 先到）都可能触发，
 * 这里造兜底节点会在会话流里留下无主幽灵块，比丢一行输出更糟。
 */
export function applyToolCallEvent(state: Draft<SessionState>, msg: ToolCallEvent): void {
  switch (msg.type) {
    case 'tool_call_begin': {
      // 幂等只对"仍 running 的同名节点"生效：那是 rollout replay 重放了未结束的调用；
      // 已终态的同名节点则说明下一轮复用了同一 call_id（DeepSeek 系 call_0/call_1 每轮从 0 重来），
      // 若一并吞掉，新一轮调用不可见、输出还会污染上一轮的历史卡片。
      const existing = findByCallId(state, msg.call_id);
      if (existing && existing.status === 'running') return;
      state.nodes.push(newToolCallNode(msg.call_id, msg.tool, msg.args));
      return;
    }
    case 'tool_call_output_delta': {
      const node = findByCallId(state, msg.call_id);
      if (!node) return;
      if (msg.channel === 'stderr') node.stderr += msg.delta;
      else node.stdout += msg.delta;
      return;
    }
    case 'tool_call_end': {
      const node = findByCallId(state, msg.call_id);
      if (!node) return;
      node.exitCode = msg.exit_code;
      node.error = msg.error;
      if (msg.result != null) {
        // result 与 status 必须在同一分支紧邻写完：turn.ts 见到齐全的权威信息就跳过轮次收敛，
        // 只写 result 漏写 status 会让节点永久停在 running、spinner 永远转。
        // 未知取值（后端灰度先发前端还没收录的结果）一律归 error 并保留原文——最保守的选择，
        // 绝不假装成功；同时必须定 state，不能留 running。
        const mapped = (STATUS_FROM_RESULT as Record<string, ToolCallStatus | undefined>)[msg.result];
        node.result = msg.result;
        node.status = mapped ?? 'error';
      } else {
        // 回退（旧事件无 result）：error 非空优先于退出码（进程可能 0 退出但工具层判失败）；
        // 此路径刻意不产出 denied——按文案反推会在措辞调整时静默失效。
        node.status = msg.error ? 'error' : msg.exit_code === 0 ? 'ok' : 'error';
      }
      return;
    }
  }
}
