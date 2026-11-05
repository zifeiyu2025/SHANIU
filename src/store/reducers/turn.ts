import type { Draft } from 'immer';
import type { EventMsg } from '../../transport/types';
import type { SessionState } from '../session-store';

type TurnEvent = Extract<EventMsg, { type: 'turn_started' | 'turn_completed' }>;

export function applyTurnEvent(state: Draft<SessionState>, msg: TurnEvent): void {
  switch (msg.type) {
    case 'turn_started':
      state.status = 'running';
      state.currentTurn = msg.turn;
      break;
    case 'turn_completed':
      state.status = 'idle';
      state.lastTurnEnd = msg.reason;
      // 冻结所有仍在流式中的节点（含 agent 消息）
      for (const node of state.nodes) {
        if (node.kind === 'agent_message') node.streaming = false;
        if (node.kind !== 'tool_call') continue;
        // 权威信息齐全（既有 result 又已离开 running）时才完全跳过轮次收敛：状态在 tool_call_end
        // 那一刻就定稿了。只判 result 不判 status 不行——万一漏写状态，跳过会让节点永久停在 running、
        // spinner 一直转（SH-017 花大力气消灭的失效形态），缺一半仍按下面的兜底收敛。
        // 也不能退回"只翻 running"：取消时后端先发 end 再发 turn_completed，节点此刻已是终态，
        // 那样收紧会让取消态退回红色"失败"（SH-017 的 P0）。
        if (node.result !== undefined && node.status !== 'running') continue;
        // 回退路径（旧事件无 result）：轮次已取消 + 无退出码 = 取消。后端取消时只发
        // ToolCallEnd{exit_code: None, error: "已取消"}（agent_loop/mod.rs 审批门控与工具执行两处），
        // EventMsg 里没有取消状态位，前端只能这样反推；已知不精确（同轮先真实失败后被取消会一并
        // 归为取消，error 原文仍保留在节点上）。
        if (msg.reason === 'cancelled' && node.exitCode == null) {
          node.status = 'cancelled';
        } else if (node.status === 'running') {
          // 无条件收敛：轮次已结束仍 running（含 completed 却漏发 end 的后端异常）时 end 不会再来，
          // 不收敛则 spinner 永久转圈
          node.status = 'error';
        }
      }
      break;
  }
}
