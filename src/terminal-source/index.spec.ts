/**
 * SH-071 的用户终端数据源契约测试。
 *
 * 这里钉住两件事：
 * 1. 前端只经 `user_terminal_*` 命令与 `user-terminal://frame` 事件驱动本地进程，
 *    不存在第二条通往模型上下文的出口（没有 gateway/会话事件/工具调用）；
 * 2. 输出帧、退出帧、尺寸协商与关闭的语义与组件期待一致。
 */
import { beforeEach, describe, expect, it, vi } from 'vitest'

const invokeMock = vi.hoisted(() => vi.fn())
const listenMock = vi.hoisted(() => vi.fn())

vi.mock('@tauri-apps/api/core', () => ({ invoke: invokeMock }))
vi.mock('@tauri-apps/api/event', () => ({ listen: listenMock }))

import { UserTerminalView } from './index'

type FrameHandler = (event: { payload: unknown }) => void

interface Harness {
  readonly view: UserTerminalView
  readonly commands: string[]
  readonly unlisten: ReturnType<typeof vi.fn>
  fire(payload: unknown): void
}

function harness(cwd: string | undefined = 'D:\\Workspace'): Harness {
  const commands: string[] = []
  const unlisten = vi.fn()
  let handler: FrameHandler | undefined
  listenMock.mockImplementation(async (name: string, callback: FrameHandler) => {
    expect(name).toBe('user-terminal://frame')
    handler = callback
    return unlisten
  })
  invokeMock.mockImplementation(async (command: string) => {
    commands.push(command)
    if (command === 'user_terminal_open') {
      return {
        id: 'terminal-1',
        title: '命令提示符',
        shell: 'C:\\Windows\\System32\\cmd.exe',
        cwd: cwd ?? 'D:\\Workspace',
        cols: 120,
        rows: 30,
        state: 'running',
        exit_code: null,
        error: null,
      }
    }
    return undefined
  })
  const view = new UserTerminalView('terminal-1', () => cwd)
  return {
    view,
    commands,
    unlisten,
    fire: payload => handler?.({ payload }),
  }
}

beforeEach(() => {
  invokeMock.mockReset()
  listenMock.mockReset()
})

describe('用户终端数据源', () => {
  it('只经 user_terminal 命令与专用帧通道驱动进程', async () => {
    const { view, commands } = harness()
    await view.refresh()

    expect(commands).toEqual(['user_terminal_open'])
    expect(invokeMock).toHaveBeenCalledWith('user_terminal_open', {
      request: expect.objectContaining({ id: 'terminal-1', cwd: 'D:\\Workspace' }),
    })
    expect(listenMock).toHaveBeenCalledWith('user-terminal://frame', expect.any(Function))
    expect(view.state.getSnapshot().phase).toBe('connected')
    expect(view.state.getSnapshot().writable).toBe(true)

    view.write('echo hi\r')
    view.resize(100, 40)
    await view.close()

    // 全量命令都落在用户终端命名空间内：没有任何会话/网关写入通道。
    expect(commands).toEqual([
      'user_terminal_open',
      'user_terminal_write',
      'user_terminal_resize',
      'user_terminal_close',
    ])
    expect(commands.every(command => command.startsWith('user_terminal_'))).toBe(true)
  })

  it('忽略别的终端输出，只把本终端的帧写进渲染状态', async () => {
    const { view, fire } = harness()
    await view.refresh()

    fire({ type: 'output', id: 'another-terminal', data: '不该出现' })
    expect(view.state.getSnapshot().render).toBeUndefined()

    fire({ type: 'output', id: 'terminal-1', data: 'hello' })
    expect(view.state.getSnapshot().render).toEqual({
      revision: 1,
      frame: { type: 'output', sequence: 1, data: 'hello' },
    })
  })

  it('收到退出帧后转为 closed 并停写', async () => {
    const { view, fire, unlisten } = harness()
    await view.refresh()
    fire({ type: 'exit', id: 'terminal-1', code: 3 })

    const state = view.state.getSnapshot()
    expect(state.phase).toBe('closed')
    expect(state.writable).toBe(false)
    expect(state.info?.state).toBe('exited')
    expect(state.info?.exitCode).toBe(3)
    expect(unlisten).not.toHaveBeenCalled()
  })

  it('尺寸不变时不重复下发 resize', async () => {
    const { view, commands } = harness()
    await view.refresh()
    view.resize(100, 40)
    view.resize(100, 40)
    expect(commands.filter(command => command === 'user_terminal_resize')).toHaveLength(1)
  })

  it('关闭后摘监听并杀进程', async () => {
    const { view, unlisten } = harness()
    await view.refresh()
    await view.close()
    expect(unlisten).toHaveBeenCalledTimes(1)
    expect(view.state.getSnapshot().phase).toBe('closed')
    expect(invokeMock).toHaveBeenCalledWith('user_terminal_close', { id: 'terminal-1' })
  })
})
