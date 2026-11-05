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
export type { DirectoryListing, DirectoryEntry } from '@deepseek-ai/dsh-host-directory-picker'
export type { SessionId } from '@deepseek-ai/dsh-session/types'

/** cordis 动态插件面（二期 runner），当前仅作类型占位。 */
export type CordisDynamicPluginId = string
export type CordisDynamicPackageId = string
export type CordisDynamicPluginRunId = string
