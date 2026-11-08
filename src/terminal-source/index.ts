/**
 * 用户终端数据源（SH-071）：把 DSH 的 `webTerminals` 服务换成 SHANIU 宿主实现。
 *
 * 界面层是上游 `ui-sidebar-terminal` 的原样移植（右侧栏槽位、标签栏形态、
 * `--dsw-alias-*` 配色都不改），这里只负责数据：
 *
 * - 进程由 `src-tauri/src/user_terminal` 起（裸进程 + ConPTY，不进沙箱）；
 * - 输入/输出走本模块自己的 Tauri 命令与 `user-terminal://frame` 事件，
 *   **不碰** `ctx.remote` / gateway / 会话事件——用户敲的内容与终端输出
 *   结构上就进不了模型上下文（docs/04 §7.4）；
 * - 不用 `unified_exec`，也不在 agent 的工具调用记录里留痕。
 */
import { Service, type Context } from '@deepseek-ai/cordis'
import { createSnapshotStore, type SnapshotStore } from '@deepseek-ai/dsh-client-store'
import type {
  TerminalEnvironment,
  TerminalLaunchShells,
  TerminalShell,
  TerminalView,
  TerminalViewState,
  WebTerminalId,
  WebTerminalInfo,
} from '@deepseek-ai/dsh-client-ui-sidebar-terminal/client'
import type { SessionId } from '@deepseek-ai/dsh-session/types'
import { invoke } from '@tauri-apps/api/core'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'

/** 宿主事件通道；名字写死在这里与 Rust 侧 `FRAME_EVENT` 保持一致。 */
const FRAME_EVENT = 'user-terminal://frame'
const SHELL_PREFERENCE_KEY = 'shaniu.userTerminal.shell'

/** Rust `TerminalInfo` 的线格式（serde 字段名保持 snake_case）。 */
interface TerminalInfoDto {
  readonly id: string
  readonly title: string
  readonly shell: string
  readonly cwd: string
  readonly cols: number
  readonly rows: number
  readonly state: 'running' | 'exited'
  readonly exit_code: number | null
  readonly error: string | null
}

interface ShellOptionDto {
  readonly path: string
  readonly name: string
  readonly args: readonly string[]
}

type FramePayload =
  | { readonly type: 'output'; readonly id: string; readonly data: string }
  | { readonly type: 'exit'; readonly id: string; readonly code: number }

interface OpenRequestDto {
  readonly id: string
  readonly shell?: string
  readonly cwd?: string
  readonly cols?: number
  readonly rows?: number
  readonly title?: string
}

/** 与宿主协商的终端上限；超出部分由 xterm 侧按此裁剪。 */
const ENVIRONMENT_LIMITS = {
  maxInputBytes: 1 << 20,
  maxBufferedBytes: 1 << 21,
  maxCols: 500,
  maxRows: 500,
  scrollback: 5000,
} as const

/**
 * 一个标签页的终端模型：与上游 `TerminalView` 同形（state/mount/refresh/
 * connect/write/resize/acknowledge/rename/close），只把数据源换成本地进程。
 */
export class UserTerminalView {
  readonly id: WebTerminalId
  readonly state: SnapshotStore<TerminalViewState> = createSnapshotStore<TerminalViewState>({
    phase: 'idle',
    writable: false,
  })
  private readonly requestedShell: string | undefined
  private readonly cwdOf: () => string | undefined
  private unlisten: UnlistenFn | undefined
  private revision = 0
  private lastSize: { cols: number; rows: number } | undefined
  private disposed = false

  constructor(
    id: WebTerminalId,
    cwdOf: () => string | undefined,
    shellPath?: string,
  ) {
    this.id = id
    this.cwdOf = cwdOf
    this.requestedShell = shellPath
  }

  /** 挂到 DOM 生命周期上；dispose 只摘监听，不杀进程。 */
  mount(): () => void {
    const snapshot = this.state.getSnapshot()
    if (snapshot.phase === 'idle') void this.refresh()
    else if (snapshot.phase === 'failed' || snapshot.phase === 'closed') void this.refresh()
    else if (this.unlisten === undefined) void this.attachListener()
    return () => { /* 进程随标签关闭/应用退出回收，DOM 卸载不杀 */ }
  }

  async refresh(): Promise<void> {
    if (this.disposed) return
    const snapshot = this.state.getSnapshot()
    if (snapshot.phase === 'loading' || snapshot.phase === 'connecting' || snapshot.phase === 'connected') return
    // 重试时整块重置：上一轮的错误/信息不该残留。
    this.state.set({ phase: 'loading', writable: false })
    // 先挂监听再起进程：ConPTY 不回放，晚订阅会丢掉首屏提示符。
    await this.attachListener()
    const cwd = this.cwdOf()
    const request: OpenRequestDto = {
      id: this.id,
      cols: this.lastSize?.cols ?? 80,
      rows: this.lastSize?.rows ?? 24,
      ...(this.requestedShell === undefined ? {} : { shell: this.requestedShell }),
      ...(cwd === undefined ? {} : { cwd }),
    }
    try {
      const info = await invoke<TerminalInfoDto>('user_terminal_open', { request })
      if (this.disposed) return
      this.lastSize = { cols: info.cols, rows: info.rows }
      this.patch({
        phase: 'connected',
        writable: true,
        info: toWebTerminalInfo(info),
        title: info.title,
        environment: environmentOf(info),
      })
    } catch (error) {
      this.fail(error)
    }
  }

  connect(): void {
    void this.attachListener()
  }

  /** 宿主侧无需按帧回执：事件到达即渲染（上游的 credits 在这里没有对应物）。 */
  acknowledge(_revision: number): void {}

  write(data: string): void {
    if (!this.state.getSnapshot().writable) return
    void invoke('user_terminal_write', { id: this.id, data }).catch(error => this.fail(error))
  }

  resize(cols: number, rows: number): void {
    const nextCols = clamp(cols, 20, ENVIRONMENT_LIMITS.maxCols)
    const nextRows = clamp(rows, 5, ENVIRONMENT_LIMITS.maxRows)
    if (this.lastSize?.cols === nextCols && this.lastSize.rows === nextRows) return
    this.lastSize = { cols: nextCols, rows: nextRows }
    const info = this.state.getSnapshot().info
    if (info !== undefined) this.patch({ info: { ...info, cols: nextCols, rows: nextRows } })
    void invoke('user_terminal_resize', { id: this.id, cols: nextCols, rows: nextRows }).catch(() => {
      // 尺寸协商失败不影响可读性：下次 fit 会再试，不额外抛错打扰用户。
    })
  }

  /** 标题只在本会话内存里生效：宿主不持久化用户改名。 */
  async rename(title: string): Promise<void> {
    const next = title.trim()
    if (next === '') return
    const info = this.state.getSnapshot().info
    this.patch({ title: next, ...(info === undefined ? {} : { info: { ...info, title: next } }) })
  }

  async close(): Promise<void> {
    this.detach()
    const snapshot = this.state.getSnapshot()
    const info = snapshot.info
    this.patch({
      phase: 'closed',
      writable: false,
      ...(info === undefined ? {} : { info: { ...info, state: 'exited', exitCode: info.exitCode ?? 0 } }),
    })
    try {
      await invoke('user_terminal_close', { id: this.id })
    } catch {
      // 进程可能已自行退出；关闭是幂等的，不把错误推给用户。
    }
  }

  async dispose(): Promise<void> {
    this.disposed = true
    await this.close()
  }

  private async attachListener(): Promise<void> {
    if (this.unlisten !== undefined || this.disposed) return
    this.unlisten = await listen<FramePayload>(FRAME_EVENT, event => {
      const payload = event.payload
      if (payload.id !== this.id) return
      if (payload.type === 'output') this.pushOutput(payload.data)
      else this.markExited(payload.code)
    })
  }

  private detach(): void {
    this.unlisten?.()
    this.unlisten = undefined
  }

  private pushOutput(data: string): void {
    if (data === '') return
    this.revision += 1
    this.patch({ render: { revision: this.revision, frame: { type: 'output', sequence: this.revision, data } } })
  }

  private markExited(code: number): void {
    const snapshot = this.state.getSnapshot()
    if (snapshot.phase === 'closed') return
    this.patch({
      phase: 'closed',
      writable: false,
      ...(snapshot.info === undefined
        ? {}
        : { info: { ...snapshot.info, state: 'exited' as const, exitCode: code } }),
    })
  }

  private fail(error: unknown): void {
    this.detach()
    const message = error instanceof Error ? error.message : String(error)
    this.patch({ phase: 'failed', writable: false, error: message })
  }

  private patch(next: Partial<TerminalViewState>): void {
    this.state.set({ ...this.state.getSnapshot(), ...next })
  }
}

/** `webTerminals` 服务：与上游 `ClientTerminals` 同名同形，宿主换成 SHANIU。 */
export class UserTerminals extends Service {
  private readonly views = new Map<string, UserTerminalView>()

  constructor(ctx: Context) {
    super(ctx, 'webTerminals')
    ctx.effect(() => () => { void this.dispose() }, 'terminal-source.lifetime')
    ctx.effect(() => {
      const unload = (): void => { void invoke('user_terminal_close_all') }
      window.addEventListener('beforeunload', unload)
      return () => window.removeEventListener('beforeunload', unload)
    }, 'terminal-source.beforeunload')
  }

  view(sessionId: SessionId, key: string, _contentId: string, terminalId?: WebTerminalId, shellPath?: string): TerminalView {
    const mapKey = `${sessionId}\u0000${key}`
    let view = this.views.get(mapKey)
    if (view === undefined) {
      view = new UserTerminalView(
        terminalId ?? newTerminalId(),
        () => sessionCwd(this.ctx, sessionId),
        shellPath,
      )
      this.views.set(mapKey, view)
    }
    // `TerminalView` 带私有字段，结构上无法直接赋值；公开面与上游一致，只需这一处断言。
    return view as unknown as TerminalView
  }

  close(sessionId: SessionId, key: string, _contentId: string, terminalId?: WebTerminalId): void {
    const mapKey = `${sessionId}\u0000${key}`
    const view = this.views.get(mapKey)
    this.views.delete(mapKey)
    if (view !== undefined) void view.close()
    else if (terminalId !== undefined) void invoke('user_terminal_close', { id: terminalId }).catch(() => {})
  }

  /** 用户终端不跨窗口持有：标签关掉就杀进程，没有后台保活语义。 */
  retainTabs(_tabs: readonly { readonly sessionId: SessionId; readonly tabId: string; readonly contentId: string }[]): void {}

  async launchShells(_sessionId: SessionId, signal: AbortSignal): Promise<TerminalLaunchShells> {
    if (signal.aborted) throw new DOMException('Aborted', 'AbortError')
    const shells = (await invoke<ShellOptionDto[]>('user_terminal_shells')).map(toTerminalShell)
    const preferred = preferredShell()
    return {
      shells,
      selectedShell: shells.find(shell => shell.path === preferred)?.path ?? shells[0]?.path,
    }
  }

  selectShell(path: string): void {
    rememberShell(path)
  }

  private async dispose(): Promise<void> {
    const views = [...this.views.values()]
    this.views.clear()
    await Promise.all(views.map(view => view.dispose()))
    await invoke('user_terminal_close_all').catch(() => {})
  }
}

/** 供 `dsh-boot.ts` 使用的特性插件形状（与其它 {inject, apply} 特性一致）。 */
export const inject: readonly string[] = []

export function apply(ctx: Context): void {
  new UserTerminals(ctx)
}

function toTerminalShell(option: ShellOptionDto): TerminalShell {
  return { path: option.path, args: [...option.args], name: option.name }
}

function toWebTerminalInfo(info: TerminalInfoDto): WebTerminalInfo {
  return {
    id: info.id as WebTerminalId,
    title: info.title,
    shell: { path: info.shell, args: [], name: shellName(info.shell) },
    cwd: info.cwd,
    cols: info.cols,
    rows: info.rows,
    state: info.state,
    exitCode: info.exit_code,
    ...(info.error === null ? {} : { error: info.error }),
  }
}

function environmentOf(info: TerminalInfoDto): TerminalEnvironment {
  return { cwd: info.cwd, ...ENVIRONMENT_LIMITS }
}

function shellName(path: string): string {
  const base = path.split(/[\\/]/).pop() ?? path
  return base.replace(/\.exe$/i, '')
}

interface SessionSummary {
  readonly cwd?: string
}

interface SessionsFace {
  readonly list: {
    getSnapshot(): { readonly byId?: Readonly<Record<string, SessionSummary>> }
  }
}

/** 起始目录跟随会话工作区；会话服务缺席（浏览器联调）就交给宿主取当前目录。 */
function sessionCwd(ctx: Context, sessionId: SessionId): string | undefined {
  const sessions = ctx.get('sessions') as SessionsFace | undefined
  const cwd = sessions?.list.getSnapshot().byId?.[sessionId]?.cwd
  return typeof cwd === 'string' && cwd !== '' ? cwd : undefined
}

function newTerminalId(): WebTerminalId {
  const id = globalThis.crypto?.randomUUID?.() ?? `${Date.now()}-${Math.random().toString(16).slice(2)}`
  return id as WebTerminalId
}

function preferredShell(): string | undefined {
  try {
    return globalThis.localStorage?.getItem(SHELL_PREFERENCE_KEY) ?? undefined
  } catch {
    return undefined
  }
}

function rememberShell(path: string): void {
  try {
    globalThis.localStorage?.setItem(SHELL_PREFERENCE_KEY, path)
  } catch {
    // 隐私模式禁用存储时退回默认 shell。
  }
}

function clamp(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value))
}
