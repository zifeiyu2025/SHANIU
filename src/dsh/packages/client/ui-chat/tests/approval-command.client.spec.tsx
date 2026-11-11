// @vitest-environment jsdom
import type { ChatSnapshot, UseChat } from '@deepseek-ai/dsh-client-ui-chat/client'
import type { PropsRuntime } from '@deepseek-ai/dsh-client-ui-slots'
import { PartialArguments } from '@deepseek-ai/dsh-util-values'
import { render, screen } from '@testing-library/react'
import { describe, expect, it } from 'vitest'
import { ApprovalCommand, commandOf } from '../src/client/chat/ApprovalCommand.tsx'

function props(
  nodes: readonly unknown[],
  callId = 'call-1',
): PropsRuntime<'conversation.approval.detail'> {
  const snapshot = {
    nodes: { values: () => nodes },
  } as unknown as ChatSnapshot
  const useChat = ((selector: (value: ChatSnapshot) => unknown) => selector(snapshot)) as UseChat
  return { callId, useChat } as PropsRuntime<'conversation.approval.detail'>
}

describe('commandOf', () => {
  it('accepts only a string command from valid JSON arguments', () => {
    expect(commandOf(undefined)).toBeUndefined()
    expect(commandOf({ callId: 'c1', argsRaw: '{' })).toBeUndefined()
    expect(commandOf({ callId: 'c1', argsRaw: '{}' })).toBeUndefined()
    expect(commandOf({ callId: 'c1', argsRaw: '{"command":42}' })).toBeUndefined()
    expect(commandOf({ callId: 'c1', argsRaw: '{"command":"pnpm test"}' })).toBe('pnpm test')
  })
})

describe('ApprovalCommand', () => {
  it('renders the running correlated Tool command', () => {
    render(<ApprovalCommand {...props([
      { kind: 'assistant-step', data: {} },
      { kind: 'tool-call', data: { root: { phase: 'start', args: PartialArguments.fromText('{"command":"wrong"}'), callId: 'other', argsRaw: '{"command":"wrong"}' } } },
      { kind: 'tool-call', data: { root: { phase: 'start', args: PartialArguments.fromText('{"command":"pnpm test"}'), callId: 'call-1', argsRaw: '{"command":"pnpm test"}' } } },
    ] as never)} />)

    expect(screen.getByText('pnpm test')).toBeTruthy()
  })

  it('omits absent, uncorrelated, preparing, and settled-without-pair Tool calls', () => {
    const { container, rerender } = render(<ApprovalCommand {...props([
      { kind: 'assistant-step', data: {} },
      { kind: 'tool-call', data: { root: undefined } },
      { kind: 'tool-call', data: { root: { phase: 'start', args: PartialArguments.fromText('{}'), callId: 'other', argsRaw: '{}' } } },
      { kind: 'tool-call', data: { root: { phase: 'preparing', args: PartialArguments.EMPTY, callId: 'call-1', name: 'bash' } } },
      {
        kind: 'tool-call',
        data: { root: { kind: 'tool-result', callId: 'call-1' } },
      },
    ] as never)} />)
    expect(container.textContent).toBe('')

    rerender(<ApprovalCommand {...props([
      { kind: 'tool-call', data: { root: { phase: 'start', args: PartialArguments.fromText('{}'), callId: 'call-1', argsRaw: '{}' } } },
    ] as never)} />)
    // 可解析但无参数：不是空白卡，明确写「无参数」
    expect(container.textContent).toContain('无参数')
  })

  it('renders a settled call when its paired call still carries the original arguments', () => {
    // 提权询问（on-failure）到达时 tool-call 往往已结束，详情仍必须可用。
    render(<ApprovalCommand {...props([
      {
        kind: 'tool-call',
        data: {
          root: {
            kind: 'tool-result',
            callId: 'call-1',
            call: { name: 'unified_exec', argsRaw: '{"command":"rm -rf build"}' },
          },
        },
      },
    ] as never)} />)
    expect(screen.getByText('rm -rf build')).toBeTruthy()
  })

  it('renders the complete command without truncation and preserves newlines', () => {
    const command = `set -e\n${'echo check; '.repeat(30)}\npnpm test`
    render(<ApprovalCommand {...props([
      {
        kind: 'tool-call',
        data: {
          root: { phase: 'start', name: 'unified_exec', args: PartialArguments.fromText(JSON.stringify({ command })), callId: 'call-1', argsRaw: JSON.stringify({ command }) },
        },
      },
    ] as never)} />)
    expect(screen.getByText((_, element) => element?.tagName === 'PRE' && element.textContent === command)).toBeTruthy()
    expect(screen.queryByText(/…/u)).toBeNull()
  })

  it('lists every patched file with operation and line counts before expanding', () => {
    const patch = [
      '*** Begin Patch',
      '*** Update File: src/a.ts',
      '@@ export function a()',
      '-old',
      '+new',
      '*** Update File: src/deep/b.ts',
      '@@ export function b()',
      '-1',
      '+2',
      '*** End Patch',
    ].join('\n')
    render(<ApprovalCommand {...props([
      {
        kind: 'tool-call',
        data: {
          root: { phase: 'start', name: 'apply_patch', args: PartialArguments.fromText(JSON.stringify({ patch })), callId: 'call-1', argsRaw: JSON.stringify({ patch }) },
        },
      },
    ] as never)} />)
    // 默认（不展开）就要能说出改哪些文件、什么操作
    expect(screen.getByText('src/a.ts')).toBeTruthy()
    expect(screen.getByText('src/deep/b.ts')).toBeTruthy()
    expect(screen.getAllByText('更新')).toHaveLength(2)
    expect(screen.getAllByText('+1')).toHaveLength(2)
    expect(screen.getAllByText('-1')).toHaveLength(2)
    // 完整补丁仍在卡片内可达
    expect(screen.getByText((_, element) => element?.tagName === 'PRE' && element.textContent === patch)).toBeTruthy()
  })

  it('fails loud instead of rendering a blank card for malformed arguments', () => {
    render(<ApprovalCommand {...props([
      {
        kind: 'tool-call',
        data: {
          root: { phase: 'start', name: 'apply_patch', args: PartialArguments.fromText('{"patch":123}'), callId: 'call-1', argsRaw: '{"patch":123}' },
        },
      },
    ] as never)} />)
    expect(screen.getByText(/参数无法解析/u)).toBeTruthy()
    // 折叠预览与展开原文各一处
    expect(screen.getAllByText('{"patch":123}')).toHaveLength(2)
  })
})
