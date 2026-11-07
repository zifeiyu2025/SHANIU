import { describe, expect, it, vi } from 'vitest'

import type { AgentEvent } from './types'
import type { HostBridge } from './tauri'
import { createWsBridge, type WebSocketLike } from './ws-bridge'

const WS_OPEN = 1
const WS_CLOSED = 3

/** 记录调用顺序的脚本化 socket：open/message/close 都由测试驱动。 */
class FakeSocket implements WebSocketLike {
  readyState = 0
  onopen: (() => void) | null = null
  onmessage: ((event: { data: unknown }) => void) | null = null
  onclose: (() => void) | null = null
  onerror: (() => void) | null = null

  constructor(private readonly log: string[]) {}

  send(data: string): void {
    this.log.push(`send:${data}`)
  }

  close(): void {
    if (this.readyState === WS_CLOSED) return
    this.readyState = WS_CLOSED
    this.onclose?.()
  }

  /** 模拟浏览器握手成功。 */
  open(): void {
    this.readyState = WS_OPEN
    this.onopen?.()
  }

  /** 模拟服务端发帧。 */
  message(frame: Record<string, unknown>): void {
    this.onmessage?.({ data: JSON.stringify(frame) })
  }

  /** 模拟服务端/网络关闭连接。 */
  serverClose(): void {
    this.readyState = WS_CLOSED
    this.onclose?.()
  }
}

interface Harness {
  bridge: HostBridge
  log: string[]
  sockets: FakeSocket[]
  dispose: () => void
  emitBroadcast: (event: AgentEvent) => void
  tauriListenerCount: () => number
  tauriUnlistenCount: () => number
}

function harness(options: {
  gatewayInfo?: () => Promise<{ port: number; token: string }>
  onEvent?: (event: AgentEvent) => void
} = {}): Harness {
  const log: string[] = []
  const sockets: FakeSocket[] = []
  let broadcastHandler: ((event: AgentEvent) => void) | undefined
  let tauriListeners = 0
  let tauriUnlistens = 0
  const base: HostBridge = {
    async invoke<T>(command: string, args?: Record<string, unknown>): Promise<T> {
      log.push(`invoke:${command}`)
      if (command === 'gateway_info') {
        return (options.gatewayInfo ?? (async () => ({ port: 4567, token: 'test-token' })))() as Promise<T>
      }
      if (command === 'create_session' || command === 'fork_session') return 'new-session' as T
      if (command === 'send_task' || command === 'resume_session') {
        return (typeof args?.agentId === 'string' && args.agentId !== '' ? args.agentId : 'new-session') as T
      }
      return undefined as T
    },
    listenEvents(handler) {
      log.push('listen:tauri')
      tauriListeners += 1
      broadcastHandler = handler
      return () => {
        tauriUnlistens += 1
        broadcastHandler = undefined
      }
    },
  }
  const bridge = createWsBridge(base, {
    socketFactory: (url) => {
      const socket = new FakeSocket(log)
      log.push(`socket:${url}`)
      sockets.push(socket)
      return socket
    },
    reconnectDelayMs: 1,
    maxInitialAttempts: 2,
    readyTimeoutMs: 100,
  })
  const dispose = bridge.listenEvents(options.onEvent ?? (() => {}))
  return {
    bridge,
    log,
    sockets,
    dispose,
    emitBroadcast: (event) => broadcastHandler?.(event),
    tauriListenerCount: () => tauriListeners,
    tauriUnlistenCount: () => tauriUnlistens,
  }
}

const tick = (ms = 5): Promise<void> => new Promise(resolve => setTimeout(resolve, ms))

function event(sessionId: string, eventId: string): AgentEvent {
  return {
    event_id: eventId,
    session_id: sessionId,
    agent_id: 'root',
    parent_id: null,
    ts_ms: 1,
    msg: { type: 'turn_started', turn: 1 },
  }
}

/** 已发出的 subscribe 帧（按发送顺序）。 */
function subscribeFrames(log: string[]): Record<string, unknown>[] {
  return log
    .filter(entry => entry.startsWith('send:'))
    .map(entry => JSON.parse(entry.slice('send:'.length)) as Record<string, unknown>)
    .filter(frame => frame.type === 'subscribe')
}

function sentFrames(log: string[]): Record<string, unknown>[] {
  return log
    .filter(entry => entry.startsWith('send:'))
    .map(entry => JSON.parse(entry.slice('send:'.length)) as Record<string, unknown>)
}

describe('ws-bridge（SH-094 批 3）', () => {
  it('gateway_info → 首帧 hello → ready 后才发 subscribe，新会话带空游标 replay', async () => {
    const h = harness()
    try {
      await tick()
      expect(h.sockets).toHaveLength(1)
      const ws = h.sockets[0]!
      ws.open()
      expect(h.log).toContain(`send:{"type":"hello","token":"test-token"}`)

      ws.message({ type: 'ready' })
      const sessionId = await h.bridge.invoke<string>('create_session', {})
      expect(sessionId).toBe('new-session')
      await h.bridge.invoke<string>('send_task', { task: 'hi', agentId: 'new-session' })

      const frames = subscribeFrames(h.log)
      expect(frames).toHaveLength(1)
      expect(frames[0]).toMatchObject({ type: 'subscribe', session_id: 'new-session', after_event_id: '' })
      // 订阅帧必须早于 send_task 命令：先注册再 spawn 任务，才没有丢首帧的窗口。
      const subscribeAt = h.log.findIndex(entry => entry.startsWith('send:') && entry.includes('"subscribe"'))
      const sendTaskAt = h.log.indexOf('invoke:send_task')
      expect(subscribeAt).toBeGreaterThanOrEqual(0)
      expect(subscribeAt).toBeLessThan(sendTaskAt)
    } finally {
      h.dispose()
    }
  })

  it('既有会话走 live-only（不带 after_event_id），重连按 lastEventId 断口补齐', async () => {
    const received: AgentEvent[] = []
    const h = harness({ onEvent: ev => received.push(ev) })
    try {
      await tick()
      const first = h.sockets[0]!
      first.open()
      first.message({ type: 'ready' })

      await h.bridge.invoke<string>('send_task', { task: 'resume', agentId: 'old-session' })
      const firstSub = subscribeFrames(h.log).at(-1)!
      expect(firstSub.session_id).toBe('old-session')
      expect(firstSub).not.toHaveProperty('after_event_id')

      first.message({ type: 'event', event: event('old-session', '01') })
      expect(received.map(ev => ev.event_id)).toEqual(['01'])

      first.serverClose()
      await tick(30)
      expect(h.sockets).toHaveLength(2)
      const second = h.sockets[1]!
      second.open()
      second.message({ type: 'ready' })
      const resub = subscribeFrames(h.log).at(-1)!
      expect(resub).toMatchObject({ session_id: 'old-session', after_event_id: '01' })
    } finally {
      h.dispose()
    }
  })

  it('observeEvents：打开会话视图即补订阅，新会话带空游标 replay，既有会话 live-only', async () => {
    const h = harness()
    try {
      await tick()
      const ws = h.sockets[0]!
      ws.open()
      ws.message({ type: 'ready' })

      h.bridge.observeEvents?.('old-session')
      h.bridge.observeEvents?.('old-session')
      let frames = subscribeFrames(h.log)
      expect(frames).toHaveLength(1)
      expect(frames[0]).toMatchObject({ session_id: 'old-session' })
      expect(frames[0]).not.toHaveProperty('after_event_id')

      await h.bridge.invoke<string>('create_session', {})
      h.bridge.observeEvents?.('new-session')
      frames = subscribeFrames(h.log)
      expect(frames).toHaveLength(2)
      expect(frames[1]).toMatchObject({ session_id: 'new-session', after_event_id: '' })
    } finally {
      h.dispose()
    }
  })

  it('打开事件流前先等连接就绪：send_task 不会早于 subscribe 帧发出', async () => {
    const h = harness()
    try {
      const pending = h.bridge.invoke<string>('send_task', { task: 'hi', agentId: 's1' })
      let settled = false
      void pending.then(() => { settled = true })
      await tick(5)
      expect(settled).toBe(false)
      expect(h.log).not.toContain('invoke:send_task')

      const ws = h.sockets[0]!
      ws.open()
      ws.message({ type: 'ready' })
      await pending
      expect(h.log.indexOf('invoke:send_task')).toBeGreaterThan(
        h.log.findIndex(entry => entry.startsWith('send:') && entry.includes('"subscribe"')),
      )
    } finally {
      h.dispose()
    }
  })

  it('credit 低于阈值补窗；服务端事件按会话投递', async () => {
    const received: AgentEvent[] = []
    const h = harness({ onEvent: ev => received.push(ev) })
    try {
      await tick()
      const ws = h.sockets[0]!
      ws.open()
      ws.message({ type: 'ready' })
      await h.bridge.invoke<string>('create_session', {})
      await h.bridge.invoke<string>('send_task', { task: 'hi', agentId: 'new-session' })

      // 默认窗口 1024、阈值 256：灌 769 条后剩 255 → 补 1024。
      for (let index = 0; index < 769; index += 1) {
        ws.message({ type: 'event', event: event('new-session', `ev-${String(index).padStart(4, '0')}`) })
      }
      expect(received).toHaveLength(769)
      const stats = (globalThis as Record<string, unknown>).__SHANIU_GATEWAY_STATS__ as
        | { events: number; transport: string }
        | undefined
      expect(stats?.events).toBe(769)
      expect(stats?.transport).toBe('ws')
      const creditFrame = sentFrames(h.log).find(frame => frame.type === 'credit')
      expect(creditFrame).toMatchObject({ type: 'credit', session_id: 'new-session', credits: 1024 })
    } finally {
      h.dispose()
    }
  })

  it('gateway 连续不可用后只退回一次 Tauri 广播，之后不再重连', async () => {
    const attempts = vi.fn(async () => {
      throw new Error('gateway 尚未绑定端口')
    })
    const h = harness({ gatewayInfo: attempts })
    const received: AgentEvent[] = []
    h.bridge.listenEvents(ev => received.push(ev))
    try {
      await tick(60)
      expect(h.tauriListenerCount()).toBe(1)
      expect(attempts.mock.calls.length).toBe(2)
      expect(h.sockets).toHaveLength(0)

      h.emitBroadcast(event('s1', '01'))
      expect(received.map(ev => ev.event_id)).toEqual(['01'])

      // 退回后不再重试 gateway_info；也不会因为断线再建 socket。
      await tick(30)
      expect(attempts.mock.calls.length).toBe(2)
      expect(h.sockets).toHaveLength(0)
    } finally {
      h.dispose()
      expect(h.tauriUnlistenCount()).toBeGreaterThanOrEqual(1)
    }
  })

  it('dispose 关连接、清订阅并停止重连', async () => {
    const h = harness()
    await tick()
    const ws = h.sockets[0]!
    ws.open()
    ws.message({ type: 'ready' })
    await h.bridge.invoke<string>('create_session', {})
    await h.bridge.invoke<string>('send_task', { task: 'hi', agentId: 'new-session' })

    h.dispose()
    expect(ws.readyState).toBe(WS_CLOSED)
    await tick(30)
    expect(h.sockets).toHaveLength(1)
    expect(h.tauriListenerCount()).toBe(0)
  })
})
