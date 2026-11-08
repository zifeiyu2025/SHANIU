/**
 * 终端模型契约（与上游 `@deepseek-ai/dsh-api-terminal-controller` 的公开类型同形）。
 *
 * 上游把契约定义在终端控制器包里，而 SHANIU 的数据源是宿主进程而不是 gateway，
 * 直接引那个包会把它的 gateway/remote 代码拖进本工程的类型图。这里只保留
 * 组件实际用到的结构，字段名与语义逐项对齐上游，数据源实现（`src/terminal-source`）
 * 按同一份契约填充。
 */
import type { HostObservable } from '@deepseek-ai/dsh-client-ui-slots'
import type { SessionId } from '@deepseek-ai/dsh-session/types'

/** 宿主侧终端标识（前端生成、标签页生命周期内稳定）。 */
export type WebTerminalId = string

/** 一个可发现的 shell（`pwsh.exe` 不存在时列表里就不会有它）。 */
export interface TerminalShell {
  readonly path: string
  readonly args: readonly string[]
  readonly name: string
}

/** 宿主返回的终端信息。 */
export interface WebTerminalInfo {
  readonly id: WebTerminalId
  readonly title: string
  readonly shell: TerminalShell
  readonly cwd: string
  readonly cols: number
  readonly rows: number
  readonly state: 'running' | 'exited' | 'failed'
  readonly exitCode: number | null
  readonly error?: string
}

/** 终端能力上限；xterm 侧据此裁剪尺寸与回滚。 */
export interface TerminalEnvironment {
  readonly cwd?: string
  readonly maxInputBytes: number
  readonly maxBufferedBytes: number
  readonly maxCols: number
  readonly maxRows: number
  readonly scrollback: number
}

/** 连接期首帧：整屏快照（本宿主暂不产生，保留形状以对齐上游渲染分支）。 */
export interface TerminalSnapshotFrame {
  readonly type: 'snapshot'
  readonly sequence: number
  readonly screen: string
  readonly info: WebTerminalInfo
  readonly writable: boolean
}

/** 增量输出帧。 */
export interface TerminalOutputFrame {
  readonly type: 'output'
  readonly sequence: number
  readonly data: string
}

export type TerminalRenderFrame = TerminalSnapshotFrame | TerminalOutputFrame

/** 一帧渲染结果；`revision` 单调递增，组件只应用更大的那一帧。 */
export interface TerminalRender {
  readonly revision: number
  readonly frame: TerminalRenderFrame
}

export type TerminalPhase =
  | 'idle'
  | 'loading'
  | 'creating'
  | 'connecting'
  | 'connected'
  | 'disconnected'
  | 'closing'
  | 'closed'
  | 'failed'

/** 可解释的异常态（文案键与 `locales.ts` 对齐）。 */
export type TerminalViewIssue = 'missingTerminal'

/** 终端对组件可见的全部状态。 */
export interface TerminalViewState {
  readonly phase: TerminalPhase
  readonly writable: boolean
  readonly title?: string
  readonly environment?: TerminalEnvironment
  readonly info?: WebTerminalInfo
  readonly render?: TerminalRender
  readonly error?: string
  readonly issue?: TerminalViewIssue
}

/** 后台清理失败项（上游为此保留了一个独立 seat；本宿主暂不产生）。 */
export interface TerminalCloseFailure {
  readonly id: WebTerminalId
  readonly title: string
  readonly message: string
}

/** shell 发现结果与上次选择。 */
export interface TerminalLaunchShells {
  readonly shells: readonly TerminalShell[]
  readonly selectedShell?: string
}

/** 一个标签页的终端模型（组件只通过这个面驱动终端）。 */
export interface TerminalView {
  readonly id: WebTerminalId
  readonly state: HostObservable<TerminalViewState>
  mount(): () => void
  refresh(): Promise<void>
  connect(): void
  acknowledge(revision: number): void
  write(data: string): void
  resize(cols: number, rows: number): void
  rename(title: string): Promise<void>
  close(): Promise<void>
}

/**
 * 终端数据源服务：上游由终端控制器（gateway + node-pty）提供，SHANIU 由
 * `src/terminal-source` 用宿主命令实现；名字与调用面保持一致，组件无需分叉。
 */
declare module '@deepseek-ai/cordis' {
  interface Context {
    webTerminals: {
      retainTabs(tabs: readonly {
        readonly sessionId: SessionId
        readonly tabId: string
        readonly contentId: string
      }[]): void
      view(
        sessionId: SessionId,
        key: string,
        contentId: string,
        terminalId?: WebTerminalId,
        shellPath?: string,
      ): TerminalView
      close(sessionId: SessionId, key: string, contentId: string, terminalId?: WebTerminalId): void
      launchShells(sessionId: SessionId, signal: AbortSignal): Promise<TerminalLaunchShells>
      selectShell(path: string): void
    }
  }
}
