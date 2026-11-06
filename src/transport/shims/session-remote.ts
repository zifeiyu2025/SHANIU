import type {
  ModelCatalog,
  SessionAttachmentRequest,
  SessionAttachmentValue,
  SessionCancelRequest,
  SessionCancelValue,
  SessionControlFrame,
  SessionCreateRequest,
  SessionCreateValue,
  SessionEventFrame,
  SessionEventsPage,
  SessionFollowFrame,
  SessionFollowRequest,
  SessionForkRequest,
  SessionForkValue,
  SessionListRequest,
  SessionListValue,
  SessionObserveEventRequest,
  SessionOpenWorkspacePathRequest,
  SessionOpenWorkspacePathValue,
  SessionPageRequest,
  SessionProjectionFrame,
  SessionProjectionsRequest,
  SessionProjectionsValue,
  SessionPromptRequest,
  SessionPromptValue,
  SessionRenameRequest,
  SessionRenameValue,
  SessionSearchRequest,
  SessionSearchValue,
  SessionSelectModelRequest,
  SessionSelectModelValue,
  SessionState,
  SessionStateRequest,
  SessionUpdateQueueRequest,
  SessionUpdateQueueValue,
  SessionWaitRequest,
  SessionWaitValue,
  SessionWorkspacePathApplication,
} from '@deepseek-ai/dsh-api-session-controller/types'
import type { RemoteResult, RemoteStreamHandle } from '@deepseek-ai/dsh-typert-protocol'

export interface SessionRemote {
  list(request: SessionListRequest, signal?: AbortSignal): Promise<RemoteResult<SessionListValue>>
  search(request: SessionSearchRequest, signal?: AbortSignal): Promise<RemoteResult<SessionSearchValue>>
  create(request: SessionCreateRequest): Promise<RemoteResult<SessionCreateValue>>
  selectModel(request: SessionSelectModelRequest): Promise<RemoteResult<SessionSelectModelValue>>
  initializeDefaultModel(): Promise<RemoteResult<void>>
  modelCatalog(): Promise<RemoteResult<ModelCatalog>>
  canOpenWorkspacePath(): Promise<RemoteResult<boolean>>
  openWorkspacePath(
    request: SessionOpenWorkspacePathRequest,
    signal?: AbortSignal,
  ): Promise<RemoteResult<SessionOpenWorkspacePathValue>>
  workspacePathApplications(
    request: { readonly path: string },
    signal?: AbortSignal,
  ): Promise<RemoteResult<readonly SessionWorkspacePathApplication[]>>
  rename(request: SessionRenameRequest): Promise<RemoteResult<SessionRenameValue>>
  fork(request: SessionForkRequest): Promise<RemoteResult<SessionForkValue>>
  prompt(request: SessionPromptRequest, signal?: AbortSignal): Promise<RemoteResult<SessionPromptValue>>
  attachment(request: SessionAttachmentRequest): Promise<RemoteResult<SessionAttachmentValue>>
  updateQueue(request: SessionUpdateQueueRequest): Promise<RemoteResult<SessionUpdateQueueValue>>
  cancel(request: SessionCancelRequest): Promise<RemoteResult<SessionCancelValue>>
  page(request: SessionPageRequest, signal?: AbortSignal): Promise<RemoteResult<SessionEventsPage>>
  events(request: SessionPageRequest, signal?: AbortSignal): Promise<RemoteResult<SessionEventsPage>>
  observeEvent(request: SessionObserveEventRequest, signal?: AbortSignal): RemoteStreamHandle<SessionEventFrame, never>
  observeProjections(request: SessionProjectionsRequest, signal?: AbortSignal): RemoteStreamHandle<SessionProjectionFrame, never>
  state(request: SessionStateRequest, signal?: AbortSignal): Promise<RemoteResult<SessionState>>
  observeState(request: SessionStateRequest, signal?: AbortSignal): RemoteStreamHandle<SessionState, never>
  wait(request: SessionWaitRequest, signal?: AbortSignal): Promise<RemoteResult<SessionWaitValue>>
  follow(request: SessionFollowRequest, signal?: AbortSignal): RemoteStreamHandle<SessionFollowFrame, never>
  projections(request: SessionProjectionsRequest, signal?: AbortSignal): Promise<RemoteResult<SessionProjectionsValue>>
  control(signal?: AbortSignal): RemoteStreamHandle<SessionControlFrame, never>
}
