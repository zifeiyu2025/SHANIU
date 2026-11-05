import { beforeEach, describe, expect, it, vi } from 'vitest';

/**
 * Tauri 传输层的 wire 形态测试。
 *
 * 为什么值得单独钉：SH-033 新加的 `load_session` / `fork_session` 是本仓第一次出现
 * **多词 snake_case 参数**的 command（`agent_id` / `keep_items`）。前端必须传
 * camelCase（`agentId` / `keepItems`），Tauri 才映射得上；写错一个字母不会编译失败、
 * 不会让类型报错，只会在真壳里变成"点了没反应"。mock 传输层永远测不到这一层，
 * 所以这里直接断言 invoke 收到的命令名与参数键。
 */
const { invoke, listen } = vi.hoisted(() => ({
  invoke: vi.fn(),
  listen: vi.fn(),
}));

vi.mock('@tauri-apps/api/core', () => ({ invoke }));
vi.mock('@tauri-apps/api/event', () => ({
  listen,
}));

import { createTauriTransport } from '../transport/tauri';

function transport() {
  listen.mockResolvedValue(() => {});
  return createTauriTransport();
}

beforeEach(() => {
  invoke.mockReset();
  listen.mockReset();
});

describe('Tauri 传输层的 command 契约', () => {
  it('新会话提交时 agentId 显式为 null，而不是省略键', async () => {
    invoke.mockResolvedValue('new-id');
    await expect(transport().sendTask('任务 A')).resolves.toBe('new-id');
    expect(invoke).toHaveBeenCalledWith('send_task', { task: '任务 A', agentId: null });
  });

  it('续写既有会话时把 agent_id 原样带过去', async () => {
    invoke.mockResolvedValue('s1');
    await transport().sendTask('继续', 's1');
    expect(invoke).toHaveBeenCalledWith('send_task', { task: '继续', agentId: 's1' });
  });

  it('sendTask 的返回值就是后端实际使用的 agent_id（新会话靠它认身份）', async () => {
    invoke.mockResolvedValue('session-7');
    const id = await transport().sendTask('任务');
    expect(id).toBe('session-7');
  });

  it('会话列表用无参 command', async () => {
    invoke.mockResolvedValue([]);
    await transport().listSessions();
    expect(invoke).toHaveBeenCalledWith('list_sessions');
  });

  it('load_session 的参数键是 camelCase 的 agentId', async () => {
    invoke.mockResolvedValue([]);
    await transport().loadSession('s1');
    expect(invoke).toHaveBeenCalledWith('load_session', { agentId: 's1' });
  });

  it('fork_session 同时带 agentId 与 keepItems 两个 camelCase 键', async () => {
    invoke.mockResolvedValue('s2');
    await expect(transport().forkSession('s1', 7)).resolves.toBe('s2');
    expect(invoke).toHaveBeenCalledWith('fork_session', { agentId: 's1', keepItems: 7 });
  });

  it('既有 command 的形态没有被 SH-033 的改动带偏', async () => {
    invoke.mockResolvedValue(undefined);
    const t = transport();
    await t.cancel();
    await t.approve('r1', 'allow');
    await t.saveSettings({ baseUrl: 'https://x/v1', model: 'm', workspaceRoot: 'D:/w', apiKey: '  k  ' });
    expect(invoke).toHaveBeenCalledWith('cancel_task');
    expect(invoke).toHaveBeenCalledWith('approve', { requestId: 'r1', decision: 'allow' });
    // apiKey 必须被 trim 后才下行；留空则传 null 表示"不改动已存 Key"
    expect(invoke).toHaveBeenCalledWith('save_settings', {
      baseUrl: 'https://x/v1',
      model: 'm',
      workspaceRoot: 'D:/w',
      apiKey: 'k',
    });
  });

  it('事件通道名与 Rust 侧一致', async () => {
    transport().onEvent(() => {});
    expect(listen).toHaveBeenCalledWith('shaniu://event', expect.any(Function));
  });
});
