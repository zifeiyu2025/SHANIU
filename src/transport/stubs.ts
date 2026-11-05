/**
 * 后端缺失能力的显式 stub（ADR-006 接线模型的核销清单）。
 * 规则：每个 stub 必须有 SH 任务编号归属；接线里程碑 = 从本文件删除对应条目。
 * stub 只承诺"UI 可用但不触碰真实能力"，绝不伪造成功。
 */

/** `remote` 服务最小面：事件订阅全部落空（返回解绑函数），调用一律报"未接线"。 */
export function stubRemote() {
  return {
    $on: () => () => {},
    $emit: () => false,
    $call: () => Promise.reject(new Error('stub(remote): SH-017 接线前不可用')),
  }
}

/** `configForms` 服务最小面：所有命名空间 ready + 空值，写操作拒绝。 */
export function stubConfigForms() {
  const form = {
    getSnapshot: () => ({
      status: 'ready' as const,
      value: undefined,
      base: undefined,
      user: undefined,
      revision: undefined,
      writable: false,
    }),
    subscribe: () => () => {},
    mutate: () => Promise.resolve(false),
    set: () => Promise.resolve(false),
    unset: () => Promise.resolve(false),
  }
  return {
    get: () => form,
    developerTools: { enabled: { getSnapshot: () => false, subscribe: () => () => {} } },
  }
}
