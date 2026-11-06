export type { RemoteFailure, RemoteResult } from '@deepseek-ai/dsh-typert-protocol'
export type { ClientRemote, RemoteHostFacts } from '@deepseek-ai/dsh-api-gateway/client'
export type {
  ModelSelection, ModelCatalog, ModelReasoningEffort, ModelProviderGroup,
} from '@deepseek-ai/dsh-api-session-controller/types'
export type { SettingsPathOpView, SettingsNamespaceView } from '@deepseek-ai/dsh-settings/types'
export type { DirectoryListing, DirectoryEntry } from '@deepseek-ai/dsh-host-directory-picker/types'
export type { SessionId } from '@deepseek-ai/dsh-session/types'
export type { CordisDynamicPluginId, CordisDynamicPackageId, CordisDynamicPluginRunId } from '@deepseek-ai/dsh-cordis-host-runner/types'

import type { RemoteResult, RemoteStreamHandle, RemoteDecoded } from '@deepseek-ai/dsh-typert-protocol'
import type { SessionId } from '@deepseek-ai/dsh-session/types'
import type { ToolCallId } from '@deepseek-ai/dsh-llm/brand'
import type { CredentialInfo } from '@deepseek-ai/dsh-credentials/types'
import type {
  LlmConfigurableProvider, LlmDiscoveredModel, LlmModelDiscoveryRequest, LlmProviderInfo,
} from '@deepseek-ai/dsh-llm/types'
import type {
  SettingsDescribeValue, SettingsNamespaceView, SettingsPathOpView,
} from '@deepseek-ai/dsh-settings/types'
import type { SettingsDocumentOpenValue } from '@deepseek-ai/dsh-api-settings-controller/types'
import type { PermissionCatalog } from '@deepseek-ai/dsh-permission-presets/client'
import type { DirectoryListing } from '@deepseek-ai/dsh-host-directory-picker/types'
import type * as Workspace from '@deepseek-ai/dsh-api-workspace-controller/types'
import type * as Files from '@deepseek-ai/dsh-api-workspace-files/types'
import type * as Plugins from '@deepseek-ai/dsh-plugin-manager/types'
import type * as Schedule from '@deepseek-ai/dsh-schedule/types'
import type { PluginInventorySnapshot } from '@deepseek-ai/dsh-host-plugin-inventory/types'
import type { NpmFileSource } from '@deepseek-ai/dsh-plugin-detail/types'
import type { RemoteAccessFrame } from '@deepseek-ai/dsh-api-remote-access/types'
import type { CommandDescriptor, CommandExecution, CommandSubmitAttachment } from '@deepseek-ai/dsh-commands/types'
import type { AskUserQuestionAnswer } from '@deepseek-ai/dsh-user-questions/types'
import type { SessionToolResultValue } from '@deepseek-ai/dsh-client-ui-trajectory/types'
import type { EncodedFileUploadRequest, FileUploadValue } from '@deepseek-ai/dsh-client-file-upload/types'
import type { ProductEvent } from '../../dsh/packages/client/product-analytics/src/events'
import type { OfficeToPdfPriority, RenderedDocumentBytes, OfficeToPdfGeneration } from '@deepseek-ai/dsh-office-to-pdf/types'
import type { SessionRemote } from './session-remote'
import type { CompactionCheckpointSource } from '@deepseek-ai/dsh-compaction/checkpoint'

declare module '@deepseek-ai/dsh-llm' {
  interface MessageSourceMap {
    'compact-checkpoint': CompactionCheckpointSource
  }
}

interface SettingsRemote {
  describe(): Promise<RemoteResult<SettingsDescribeValue>>
  mutate(ns: string, ops: readonly SettingsPathOpView[], expectedRevision?: number): Promise<RemoteResult<SettingsNamespaceView>>
  openSettingsDocument(signal?: AbortSignal): Promise<RemoteResult<SettingsDocumentOpenValue>>
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

interface WorkspaceRemote {
  create(request: Workspace.WorkspaceCreateRequest): Promise<RemoteResult<Workspace.WorkspaceCreateValue>>
  initializeDefault(signal?: AbortSignal): Promise<RemoteResult<Workspace.WorkspaceValue | undefined>>
  rename(request: Workspace.WorkspaceRenameRequest): Promise<RemoteResult<Workspace.WorkspaceValue>>
  delete(request: Workspace.WorkspaceDeleteRequest): Promise<RemoteResult<Workspace.WorkspaceDeleteValue>>
  insertBefore(request: Workspace.WorkspaceInsertBeforeRequest): Promise<RemoteResult<Workspace.WorkspaceOrderValue>>
  insertSessionBefore(request: Workspace.WorkspaceInsertSessionBeforeRequest): Promise<RemoteResult<Workspace.WorkspaceValue>>
  archiveSession(request: Workspace.WorkspaceArchiveSessionRequest): Promise<RemoteResult<Workspace.WorkspaceArchiveValue>>
  unarchiveSession(request: Workspace.WorkspaceUnarchiveSessionRequest): Promise<RemoteResult<Workspace.WorkspaceArchiveValue>>
  pinSession(request: Workspace.WorkspacePinSessionRequest): Promise<RemoteResult<Workspace.WorkspacePinValue>>
  unpinSession(request: Workspace.WorkspaceUnpinSessionRequest): Promise<RemoteResult<Workspace.WorkspacePinValue>>
  follow(signal?: AbortSignal): RemoteStreamHandle<Workspace.WorkspaceFollowFrame, never>
}

interface WorkspaceFilesRemote {
  read(sessionId: SessionId, path: string, range: Files.WorkspaceFileRange, signal?: AbortSignal): Promise<RemoteResult<Files.WorkspaceFileText>>
  readBytes(sessionId: SessionId, path: string, options: Files.WorkspaceByteReadOptions, signal?: AbortSignal): Promise<RemoteResult<RemoteDecoded<Files.WorkspaceFileBytes>>>
  stat(sessionId: SessionId, path: string, signal?: AbortSignal): Promise<RemoteResult<Files.WorkspaceFileStat>>
  list(sessionId: SessionId, path: string, signal?: AbortSignal): Promise<RemoteResult<Files.WorkspaceDirectoryListing>>
  changes(sessionId: SessionId, path: string, signal?: AbortSignal): RemoteStreamHandle<Files.WorkspaceFileWatchFrame, never>
}

interface PluginManagerRemote {
  readBundleDetail(name: string, language: string, options?: { npmFileSources?: NpmFileSource[] }): Promise<RemoteResult<Plugins.BundleDetail>>
  listBundles(): Promise<RemoteResult<Plugins.BundleInfo[]>>
  listPlugins(): Promise<RemoteResult<Plugins.PluginInfo[]>>
  registries(): Promise<RemoteResult<Plugins.PluginRegistries>>
  inspect(spec: string, options?: Plugins.InspectOptions, signal?: AbortSignal): Promise<RemoteResult<Plugins.PluginSpecInspection>>
  listVersions(name: string, options?: Plugins.ListVersionsOptions, signal?: AbortSignal): Promise<RemoteResult<Plugins.PluginVersionList>>
  updateBundle(name: string, version: string, options?: Plugins.UpdateBundleOptions): Promise<RemoteResult<Plugins.PluginUpdateResult>>
  setPluginEnabled(id: Plugins.PluginEntryId, enabled: boolean): Promise<RemoteResult<Plugins.ChangeResult>>
  setBundleEnabled(name: string, enabled: boolean): Promise<RemoteResult<Plugins.ChangeResult>>
  installBundle(spec: string, options?: Plugins.InstallBundleOptions): Promise<RemoteResult<Plugins.ChangeResult>>
  waitForInstall(requestId: Plugins.PluginInstallRequestId): Promise<RemoteResult<Plugins.ChangeResult | null>>
  cancelInstall(requestId: Plugins.PluginInstallRequestId): Promise<RemoteResult<Plugins.PluginInstallCancellation>>
  removeBundle(name: string): Promise<RemoteResult<Plugins.ChangeResult>>
}

interface ScheduleRemote {
  deliveryLimits(): Promise<RemoteResult<{ readonly newSessionIntervalSeconds: number }>>
  create(request: Schedule.ScheduleCreateRequest, signal?: AbortSignal): Promise<RemoteResult<Schedule.ScheduleCatalogEntry>>
  list(request: Schedule.ScheduleListRequest): Promise<RemoteResult<Schedule.ScheduleListResult>>
  catalog(): Promise<RemoteResult<Schedule.ScheduleCatalogEntry[]>>
  view(request: Schedule.ScheduleViewRequest): Promise<RemoteResult<Schedule.ScheduleViewResult>>
  history(request: Schedule.ScheduleDeliveryHistoryRequest): Promise<RemoteResult<Schedule.ScheduleDeliveryHistoryResult>>
  run(request: Schedule.ScheduleRunRequest, signal?: AbortSignal): Promise<RemoteResult<Schedule.ScheduleRunResult>>
  deleteInactive(): Promise<RemoteResult<Schedule.ScheduleDeleteInactiveResult>>
  edit(request: Schedule.ScheduleEditRequest, signal?: AbortSignal): Promise<RemoteResult<Schedule.ScheduleEditResult>>
}

export interface RemoteNamespaces {
  readonly session: SessionRemote
  readonly settings: SettingsRemote
  readonly credentials: CredentialsRemote
  readonly llm: LlmRemote
  readonly permissionPresets: { catalog(): Promise<RemoteResult<PermissionCatalog>> }
  readonly workspace: WorkspaceRemote
  readonly workspaceFiles: WorkspaceFilesRemote
  readonly pluginManager: PluginManagerRemote
  readonly pluginRegistryProbe: { fastest(): Promise<RemoteResult<string | null>> }
  readonly pluginInventory: { list(): Promise<RemoteResult<PluginInventorySnapshot>> }
  readonly schedule: ScheduleRemote
  readonly directoryPicker: {
    pick(signal?: AbortSignal): Promise<RemoteResult<string | null>>
    list(path: string | undefined, signal?: AbortSignal): Promise<RemoteResult<DirectoryListing>>
    createDirectory(path: string, name: string): Promise<RemoteResult<string>>
  }
  readonly remoteAccess: { follow(signal?: AbortSignal): RemoteStreamHandle<RemoteAccessFrame, never> }
  readonly commands: {
    list(sessionId: SessionId): Promise<RemoteResult<readonly CommandDescriptor[]>>
    execute(sessionId: SessionId, line: string, attachments: readonly CommandSubmitAttachment[], signal?: AbortSignal): Promise<RemoteResult<CommandExecution | undefined>>
  }
  readonly userQuestions: {
    answer(sessionId: SessionId, callId: ToolCallId, answer: AskUserQuestionAnswer): Promise<RemoteResult<boolean>>
    attachWait(sessionId: SessionId, callId: ToolCallId, signal?: AbortSignal): RemoteStreamHandle<{ remainingMs: number }, never>
  }
  readonly toolResults: { watch(request: { sessionId: SessionId; seq: number }, signal?: AbortSignal): RemoteStreamHandle<SessionToolResultValue, never> }
  readonly fileUploads: { upload(sessionId: SessionId, request: EncodedFileUploadRequest, signal?: AbortSignal): Promise<RemoteResult<FileUploadValue>> }
  readonly productAnalytics: {
    report(event: ProductEvent): Promise<RemoteResult<void>>
    watchPolicy(signal?: AbortSignal): RemoteStreamHandle<boolean, never>
  }
  readonly officeToPdf: {
    render(sessionId: SessionId, path: string, priority: OfficeToPdfPriority, signal?: AbortSignal): Promise<RemoteResult<RemoteDecoded<RenderedDocumentBytes>>>
    generation(signal?: AbortSignal): Promise<RemoteResult<OfficeToPdfGeneration>>
  }
}

type Endpoint = {
  [Namespace in keyof RemoteNamespaces]: `${Namespace}/${Extract<keyof RemoteNamespaces[Namespace], string>}`
}[keyof RemoteNamespaces]

type RemoteEndpoints = {
  [Key in Endpoint]: Key extends `${infer Namespace extends keyof RemoteNamespaces}/${infer Method}`
    ? RemoteNamespaces[Namespace][Extract<Method, keyof RemoteNamespaces[Namespace]>]
    : never
}

declare module '@deepseek-ai/dsh-typert-protocol' {
  interface TypertRemoteNamespaceMap extends RemoteNamespaces {}
  interface TypertRemoteMap extends RemoteEndpoints {}
}
