/**
 * WS-backed 宿主桥（SH-094 批 3，ADR-005「内部传输路线」）。
 *
 * 装饰一个基础 HostBridge（桌面是 `tauriBridge()`）：命令面原样委托 Tauri invoke，
 * 事件面从 Tauri 全局广播切到 loopback WebSocket gateway（批 2 `gateway.rs`）。
 * 投递的仍是 `AgentEvent` 信封，适配层 `onEvent` 无需改动——它仍按 session_id 懒建
 * journal、follow 用 `load_session` 档案重建历史（已测的 SH-033 路径不动）。
 *
 * 订阅触发与续播口径（2026-11-07 用户拍板「直播+断口补齐」）：
 * - 新建会话（`create_session` / `fork_session` 返回的 id）：在 `send_task`/`resume_session`
 *   **invoke 之前**订阅，`after_event_id=""` 全量 replay 事件日志。新档案的事件日志要么为空、
 *   要么只含本次首回合的早期事件；replay 兜底「任务先于订阅发帧」的竞态。适配层在
 *   `submitPromptNow` 里已乐观建 journal（只推 user/message + turn/start），replay 的
 *   turn_started 因 taskOpen 已开成 no-op、模型 deltas/tools 与乐观帧不重叠——无重计。
 * - 既有会话（本次进程内打开过的会话）：live-only 订阅（省略 after_event_id）。历史由
 *   档案重建/本地 journal 提供，live 只投递本次任务的新事件，避免 replay 造成重计。
 * - 首次订阅前先等连接就绪（`waitForReady`）：subscribe 帧先于命令发出，命令才 spawn
 *   任务，服务端按帧序注册后事件才可能发布，live-only 才不是"看运气"。
 * - 重连：按各会话 `lastEventId` 断口补齐（gateway 从事件日志 replay 游标之后的信封）。
 * - credit：每会话初始窗口 + 低于阈值即补窗（适配层同步折叠事件，消费快，不堆积）。
 * - gateway 不可用（绑定失败/握手反复失败）时退回 Tauri 全局广播：只退一次、退后不再
 *   重连，避免两条通道同时投递造成重复；`gateway.rs` 绑定失败时 Tauri emit 仍在，
 *   所以这条兜底路径确实能收到事件。
 *
 * 测试注入 `socketFactory` 脚本化 socket；桌面默认用全局 WebSocket。
 */
import type { AgentEvent } from './types'
import type { HostBridge } from './tauri'

/** 桥用到的 WebSocket 最小子集（全局 WebSocket 与测试假 socket 都满足）。 */
export interface WebSocketLike {
  send(data: string): void
  close(): void
  readyState: number
  onopen: (() => void) | null
  onmessage: ((event: { data: unknown }) => void) | null
  onclose: (() => void) | null
  onerror: (() => void) | null
}

export interface WsBridgeOptions {
  /** socket 构造器（测试注入；默认全局 WebSocket）。 */
  socketFactory?: (url: string) => WebSocketLike
  /** 重连退避基数（测试设 0 即时重连）；实际延迟按指数退避封顶 8 倍。 */
  reconnectDelayMs?: number
  /** 每会话初始 credit 窗口。 */
  initialCredit?: number
  /** 剩余 credit 低于此值即补窗。 */
  creditRefillThreshold?: number
  /** 每次补窗追加的 credit。 */
  creditRefillAmount?: number
  /** 首次连接连续失败多少次后退回 Tauri 广播（默认 3）。 */
  maxInitialAttempts?: number
  /** 首次订阅前等待连接就绪的上限（默认 3000ms）；超时照常发命令，不把用户卡死。 */
  readyTimeoutMs?: number
}

interface GatewayInfo {
  port: number
  token: string
}

interface Subscription {
  /** "" = 新会话全量 replay；null = 既有会话 live-only。重连优先用 lastEventId。 */
  initialCursor: string | null
  lastEventId: string | undefined
  credits: number
}

const WS_OPEN = 1
const WS_CLOSED = 3

export function createWsBridge(base: HostBridge, options: WsBridgeOptions = {}): HostBridge {
  const socketFactory = options.socketFactory ?? ((url: string) => new WebSocket(url) as unknown as WebSocketLike)
  const reconnectDelayMs = options.reconnectDelayMs ?? 500
  const initialCredit = options.initialCredit ?? 1024
  const refillThreshold = options.creditRefillThreshold ?? 256
  const refillAmount = options.creditRefillAmount ?? 1024
  const maxInitialAttempts = options.maxInitialAttempts ?? 3
  const readyTimeoutMs = options.readyTimeoutMs ?? 3000

  let socket: WebSocketLike | undefined
  let ready = false
  let disposed = false
  /** 'ws' = loopback gateway；'fallback' = Tauri 全局广播（只进不退）。 */
  const status: { transport: 'ws' | 'fallback' } = { transport: 'ws' }
  /** 函数调用阻断 TS 的窄化：status 在 await 期间可能被 activateFallback 改写。 */
  const isFallback = (): boolean => status.transport === 'fallback'
  /** 连接进行中：防止 reconnect 定时器与 listenEvents 并发建连。 */
  let connecting = false
  /** 曾经握手成功过：之后的断线是正常重连，不计入初始失败。 */
  let everReady = false
  let initialFailures = 0
  let attempt = 0
  let reconnectTimer: ReturnType<typeof setTimeout> | undefined
  let fallbackUnlisten: (() => void) | undefined
  let handler: ((event: AgentEvent) => void) | undefined
  /** 本次进程新建/分叉出来的会话：首个任务用全量 replay，其余会话 live-only。 */
  const freshSessions = new Set<string>()
  const subscriptions = new Map<string, Subscription>()
  const readyWaiters: (() => void)[] = []

  /**
   * 开发期传输取证（SH-094 验收用，`import.meta.env.DEV` 静态裁剪，不进生产包）：
   * 事件从宿主采集时钟（`ts_ms`）到适配层收到的墙钟差，用于核对 FF-5 的本地传输开销。
   */
  const diagnosticsOn = import.meta.env.DEV
  const diagnostics = {
    transport: 'ws' as 'ws' | 'fallback',
    events: 0,
    reconnects: 0,
    lastLagMs: 0,
    maxLagMs: 0,
    sessions: {} as Record<string, { cursor: string | null; last: string | undefined; events: number }>,
  }
  if (diagnosticsOn) {
    ;(globalThis as Record<string, unknown>).__SHANIU_GATEWAY_STATS__ = diagnostics
    // 真机断线续播取证：主动断开当前连接，走正常 onclose → 退避重连 → 带 lastEventId 续播。
    ;(globalThis as Record<string, unknown>).__SHANIU_GATEWAY_DROP__ = () => { socket?.close() }
  }

  const flushReadyWaiters = (): void => {
    const waiters = readyWaiters.splice(0)
    for (const resolve of waiters) resolve()
  }

  const send = (frame: Record<string, unknown>): void => {
    if (socket !== undefined && socket.readyState === WS_OPEN) socket.send(JSON.stringify(frame))
  }

  const subscribeFrame = (sessionId: string, cursor: string | null): void => {
    const frame: Record<string, unknown> = { type: 'subscribe', session_id: sessionId, credits: initialCredit }
    // cursor=null → 省略字段（gateway 视为 live-only）；""/id → 带上（replay/断口补齐）。
    if (cursor !== null) frame.after_event_id = cursor
    send(frame)
  }

  /** 登记订阅；连接已就绪则立即发 subscribe，否则等 ready 后统一 flush。 */
  const ensureSubscription = (sessionId: string, initialCursor: string | null): void => {
    if (subscriptions.has(sessionId)) return
    subscriptions.set(sessionId, { initialCursor, lastEventId: undefined, credits: initialCredit })
    if (diagnosticsOn) diagnostics.sessions[sessionId] = { cursor: initialCursor, last: undefined, events: 0 }
    if (ready) subscribeFrame(sessionId, initialCursor)
  }

  /** 等连接就绪（或退回 Tauri 兜底/超时）。首次订阅必须在命令之前完成。 */
  const waitForReady = (): Promise<void> => new Promise(resolve => {
    if (ready || isFallback() || disposed) {
      resolve()
      return
    }
    let settled = false
    const done = (): void => {
      if (settled) return
      settled = true
      resolve()
    }
    readyWaiters.push(done)
    setTimeout(done, readyTimeoutMs)
  })

  /** ready 握手完成：首连 flush 待订阅；重连按 lastEventId 断口补齐并重置 credit 窗口。 */
  const onReady = (): void => {
    if (everReady && diagnosticsOn) diagnostics.reconnects += 1
    ready = true
    everReady = true
    initialFailures = 0
    attempt = 0
    for (const [sessionId, sub] of subscriptions) {
      sub.credits = initialCredit
      subscribeFrame(sessionId, sub.lastEventId ?? sub.initialCursor)
    }
    flushReadyWaiters()
  }

  const deliver = (event: AgentEvent): void => {
    if (diagnosticsOn) {
      diagnostics.events += 1
      const lag = Date.now() - event.ts_ms
      if (Number.isFinite(lag) && lag >= 0) {
        diagnostics.lastLagMs = lag
        if (lag > diagnostics.maxLagMs) diagnostics.maxLagMs = lag
      }
    }
    const sub = subscriptions.get(event.session_id)
    if (sub !== undefined) {
      if (sub.lastEventId === undefined || event.event_id > sub.lastEventId) sub.lastEventId = event.event_id
      if (diagnosticsOn) {
        const diag = diagnostics.sessions[event.session_id]
        if (diag !== undefined) { diag.last = sub.lastEventId; diag.events += 1 }
      }
      sub.credits -= 1
      if (sub.credits <= refillThreshold) {
        send({ type: 'credit', session_id: event.session_id, credits: refillAmount })
        sub.credits += refillAmount
      }
    }
    handler?.(event)
  }

  /** gateway 不可用：一次性退回 Tauri 全局广播，之后不再重连（两条通道同时投递=重复）。 */
  const activateFallback = (reason: string): void => {
    if (disposed || isFallback()) return
    status.transport = 'fallback'
    if (diagnosticsOn) diagnostics.transport = 'fallback'
    ready = false
    connecting = false
    if (reconnectTimer !== undefined) {
      clearTimeout(reconnectTimer)
      reconnectTimer = undefined
    }
    socket?.close()
    socket = undefined
    console.warn(`[SHANIU] WebSocket gateway 不可用，事件通道退回 Tauri 广播：${reason}`)
    if (handler !== undefined) fallbackUnlisten = base.listenEvents(handler)
    flushReadyWaiters()
  }

  const countInitialFailure = (): void => {
    if (everReady) return
    initialFailures += 1
    if (initialFailures >= maxInitialAttempts) activateFallback(`连续 ${initialFailures} 次连接失败`)
  }

  const scheduleReconnect = (): void => {
    if (disposed || isFallback() || reconnectTimer !== undefined) return
    const delay = reconnectDelayMs * Math.min(2 ** attempt, 8)
    attempt += 1
    reconnectTimer = setTimeout(() => {
      reconnectTimer = undefined
      void connect()
    }, delay)
  }

  const connect = async (): Promise<void> => {
    if (disposed || isFallback() || connecting) return
    connecting = true
    let info: GatewayInfo
    try {
      info = await base.invoke<GatewayInfo>('gateway_info', {})
    } catch (error) {
      connecting = false
      // gateway 尚未绑定端口（启动竞态）或绑定失败：退避重试，连续失败后退回 Tauri。
      console.error('[SHANIU] gateway_info 失败，将重试连接', error)
      countInitialFailure()
      scheduleReconnect()
      return
    }
    connecting = false
    if (disposed || isFallback()) return
    const ws = socketFactory(`ws://127.0.0.1:${info.port}`)
    socket = ws
    ready = false
    ws.onopen = () => {
      if (ws !== socket) return
      ws.send(JSON.stringify({ type: 'hello', token: info.token }))
    }
    ws.onmessage = (ev) => {
      if (ws !== socket) return
      let frame: { type?: string; event?: AgentEvent; code?: string; message?: string }
      try {
        frame = JSON.parse(String(ev.data)) as typeof frame
      } catch {
        return
      }
      if (!ready) {
        if (frame.type === 'ready') onReady()
        else if (frame.type === 'error') console.error('[SHANIU] gateway 握手错误', frame.code, frame.message)
        return
      }
      if (frame.type === 'event' && frame.event !== undefined) deliver(frame.event)
      else if (frame.type === 'error') {
        // overflow 等：服务端随后关连接，onclose 触发带游标重连续播。
        console.error('[SHANIU] gateway 错误', frame.code, frame.message)
      }
    }
    ws.onclose = () => {
      if (ws !== socket) return
      socket = undefined
      ready = false
      if (disposed) return
      if (!everReady) countInitialFailure()
      scheduleReconnect()
    }
    ws.onerror = () => { /* onclose 随后触发重连 */ }
  }

  const invoke = async <T>(command: string, args?: Record<string, unknown>): Promise<T> => {
    if (command === 'create_session' || command === 'fork_session') {
      const sessionId = await base.invoke<string>(command, args)
      // 只登记"新会话"事实；订阅推迟到首个任务前，既拿到 replay 光标又不在空会话上占资源。
      if (typeof sessionId === 'string' && sessionId !== '') freshSessions.add(sessionId)
      return sessionId as T
    }
    if (command === 'send_task' || command === 'resume_session') {
      const agentId = typeof args?.agentId === 'string' ? args.agentId.trim() : ''
      await waitForReady()
      if (agentId !== '') {
        // 新会话 replay 兜底首帧竞态；既有会话 live-only，历史交给档案/本地 journal。
        ensureSubscription(agentId, freshSessions.has(agentId) ? '' : null)
        return base.invoke<T>(command, args)
      }
      const newId = await base.invoke<string>(command, args)
      if (typeof newId === 'string' && newId !== '') {
        freshSessions.add(newId)
        ensureSubscription(newId, '')
      }
      return newId as T
    }
    return base.invoke<T>(command, args)
  }

  /**
   * 适配层打开会话视图时登记订阅（SH-094 批 3）：不调用它的话，页面重载后
   * `follow` 只会重建档案、不会让 gateway 往这条连接推实时帧（运行中任务的后半段
   * 要等到下次 send_task 才回来）。新会话用空游标 replay，既有会话 live-only。
   */
  const observeEvents = (sessionId: string): void => {
    if (sessionId === '') return
    ensureSubscription(sessionId, freshSessions.has(sessionId) ? '' : null)
  }

  const listenEvents = (h: (event: AgentEvent) => void): (() => void) => {
    handler = h
    if (isFallback()) {
      fallbackUnlisten = base.listenEvents(h)
    } else {
      void connect()
    }
    return () => {
      disposed = true
      handler = undefined
      if (reconnectTimer !== undefined) {
        clearTimeout(reconnectTimer)
        reconnectTimer = undefined
      }
      fallbackUnlisten?.()
      fallbackUnlisten = undefined
      if (socket !== undefined && socket.readyState !== WS_CLOSED) socket.close()
      socket = undefined
      ready = false
      subscriptions.clear()
      freshSessions.clear()
      flushReadyWaiters()
    }
  }

  return { invoke, listenEvents, observeEvents }
}
