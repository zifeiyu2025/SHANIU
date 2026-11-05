import { vi } from 'vitest';
import type { Settings, Transport } from '../transport/types';

/**
 * 测试用 Transport 桩：只给各方法一个中性默认，调用方按需 overrides。
 *
 * 集中成一处的理由不是"少写几行"，而是 Transport 每加一个方法，散在多个测试文件里的
 * 字面量桩会同时编译失败——那种失败与被测行为无关，却会把回归的真实信号淹掉。
 */
export function stubTransport(
  overrides: Partial<Transport> = {},
  settings: Partial<Settings> = {},
): Transport {
  const base: Settings = {
    baseUrl: 'https://mock.local/v1',
    model: 'M0-Mock',
    workspaceRoot: 'D:/MyWorkspace/demo',
    hasApiKey: false,
    sandboxMode: 'read_only',
    approvalMode: 'ask',
    ...settings,
  };
  return {
    ping: vi.fn().mockResolvedValue('n'),
    // sendTask 返回本次使用的 agent_id（SH-033）：桩必须给字符串，
    // 解析出 undefined 会让 store 以为"还没建档"，下一次提交又新建一份。
    sendTask: vi.fn().mockResolvedValue('stub-session'),
    cancel: vi.fn().mockResolvedValue(undefined),
    listSessions: vi.fn().mockResolvedValue([]),
    loadSession: vi.fn().mockResolvedValue([]),
    forkSession: vi.fn().mockResolvedValue('stub-fork'),
    getSettings: vi.fn().mockResolvedValue(base),
    saveSettings: vi.fn().mockResolvedValue(undefined),
    approve: vi.fn().mockResolvedValue(undefined),
    onEvent: vi.fn().mockReturnValue(() => {}),
    ...overrides,
  };
}
