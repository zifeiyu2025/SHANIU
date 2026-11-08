/**
 * `@deepseek-ai/node-addon-system/landlock-run` 的显式防护桩（非上游代码）。
 *
 * Landlock 是 Linux 专有的 LSM；本产品只发布 Windows 构建，原生平台包
 * （`@deepseek-ai/node-addon-system-<platform>-<arch>`）从未 vendor。上游的
 * 映射表与 `sandbox-local` 的静态 import 都还在，所以 Vite 侧用这个桩接住：
 * 模块解析成功，任何实际调用都抛一句说得清的错误，而不是晦涩的解析失败。
 *
 * 只做防护，不提供降级行为——Windows 的沙箱链是 `windows-acl`（docs/01 ADR-007）。
 * 裁剪依据见 docs/06-DSH裁剪决策记录.md「已知残缺与例外」。
 */

/** 与上游同名常量，保持导入侧形状不变（值本身在 Windows 无意义）。 */
export const LAUNCHER_BIN = 'landlock-run'

/** 与上游同名常量，保持导入侧形状不变。 */
export const LAUNCHER_FAILURE_EXIT = 125

/** 上游探测结论。桩不会返回任何值，保留类型只为导入侧不报错。 */
export type LandlockEnforcement = 'full' | 'partial' | 'unusable'

/** 上游授权描述。 */
export interface LauncherGrants {
  readonly readOnly?: readonly string[]
  readonly readWrite?: readonly string[]
}

const UNAVAILABLE =
  'landlock-run 不可用：Landlock 是 Linux 专有 LSM，本构建不提供该启动器。' +
  'Windows 请走 windows-acl 沙箱链；裁剪依据见 docs/06-DSH裁剪决策记录.md。'

function unavailable(): never {
  throw new Error(UNAVAILABLE)
}

/** 上游接口的防护实现：一调用即失败。 */
export function launcherPath(): string {
  return unavailable()
}

/** 上游接口的防护实现：一调用即失败。 */
export function grantArgs(_grants: LauncherGrants): string[] {
  return unavailable()
}

/** 上游接口的防护实现：一调用即失败。 */
export function probe(_launcher?: string, _options?: { timeoutMs?: number }): LandlockEnforcement {
  return unavailable()
}
