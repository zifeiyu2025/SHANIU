/**
 * `dsh-api-remotes/client` 聚合包的 shim：原包是"一个门面再导出全部 remote 生成物类型"。
 * SHANIU 侧改为：公共词汇从真实来源再导出（typert-protocol / gateway / 各领域包），
 * remote 命名空间面由 transport/shims/*-remote.ts 按接线进度逐个补。
 */
export type { RemoteFailure, RemoteResult } from '@deepseek-ai/dsh-typert-protocol'
export type { ClientRemote, RemoteHostFacts } from '@deepseek-ai/dsh-api-gateway/client'
export type {
  ModelSelection, ModelCatalog, ModelReasoningEffort, ModelProviderGroup,
} from '@deepseek-ai/dsh-api-session-controller/types'
export type { SettingsPathOpView, SettingsNamespaceView } from '@deepseek-ai/dsh-settings/types'
export type { DirectoryListing, DirectoryEntry } from '@deepseek-ai/dsh-host-directory-picker/types'
export type { SessionId } from '@deepseek-ai/dsh-session/types'
import type { RemoteResult } from '@deepseek-ai/dsh-typert-protocol'
import type { CredentialInfo } from '@deepseek-ai/dsh-credentials/types'
import type {
  LlmConfigurableProvider, LlmDiscoveredModel, LlmModelDiscoveryRequest, LlmProviderInfo,
} from '@deepseek-ai/dsh-llm/types'
import type {
  SettingsDescribeValue, SettingsNamespaceView, SettingsPathOpView,
} from '@deepseek-ai/dsh-settings/types'
import type { PermissionCatalog } from '@deepseek-ai/dsh-permission-presets/client'

/**
 * 已接线命名空间的 `ctx.remote.<ns>` 面。上游由 typert generator 从宿主服务生成，
 * 生成物不在树内（docs/05 SH-005 注记里的 typecheck 已知项根因）；这里按接线进度手写。
 * 只声明 transport/mock.ts 真正应答的方法，且一律 optional：
 * ClientRemoteService 并未实现它们，非 optional 会让 gateway 的 implements 直接报错。
 * `remote.session` 不在此列——它的完整面由 session-controller 消费，
 * 写一个窄面会把那批调用点全带崩，宁可留在已知项里。
 */
interface SettingsRemote {
  describe(): Promise<RemoteResult<SettingsDescribeValue>>
  mutate(
    ns: string,
    ops: readonly SettingsPathOpView[],
    expectedRevision?: number,
  ): Promise<RemoteResult<SettingsNamespaceView>>
}

interface CredentialsRemote {
  describe(refs: readonly string[]): Promise<RemoteResult<Record<string, CredentialInfo>>>
  set(ref: string, value: string): Promise<RemoteResult<void>>
  unset(ref: string): Promise<RemoteResult<void>>
}

interface LlmRemote {
  listProviders(): Promise<RemoteResult<LlmProviderInfo[]>>
  listConfigurableProviders(): Promise<RemoteResult<LlmConfigurableProvider[]>>
  discoverModels(settingsNs: string, request: LlmModelDiscoveryRequest): Promise<RemoteResult<LlmDiscoveredModel[]>>
}

interface PermissionPresetsRemote {
  catalog(): Promise<RemoteResult<PermissionCatalog>>
}

declare module '@deepseek-ai/dsh-api-gateway/client' {
  interface ClientRemote {
    readonly settings?: SettingsRemote
    readonly credentials?: CredentialsRemote
    readonly llm?: LlmRemote
    readonly permissionPresets?: PermissionPresetsRemote
  }
}

/** cordis 动态插件面（二期 runner），当前仅作类型占位。 */
export type CordisDynamicPluginId = string
export type CordisDynamicPackageId = string
export type CordisDynamicPluginRunId = string
