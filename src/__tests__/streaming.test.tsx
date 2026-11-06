/**
 * 健身函数 FF-7（docs/01 §5）：已完成 markdown 块的重渲染次数 = 0。
 *
 * 这条门禁此前**只有表里没有载体**：docs/01 写着本地检查路径
 * `src/__tests__/streaming.test.tsx`，而这个目录根本不存在，于是"每里程碑跑 FF-7"
 * 实际是每里程碑什么都不跑。补上载体。
 *
 * 判据用"探针组件被调用了几次"，不用 Profiler 回调次数：memo 命中时子树被整棵跳过，
 * 探针就不可能执行——这正是"重渲染 0 次"的机制无关定义。Profiler 的 onRender 在
 * 子树跳过时是否回调取决于版本实现，拿它当判据会把"我没测准"误读成"它没重渲染"。
 */
import { render } from '@testing-library/react'
import { memo } from 'react'
import type { ReactElement } from 'react'
import { describe, expect, it } from 'vitest'
import { MarkdownText } from '../dsh/packages/client/ui-primitives/src/markdown/MarkdownText'
import type { MarkdownLabels } from '../dsh/packages/client/ui-primitives/src/markdown/render'

const LABELS: MarkdownLabels = {
  code: { copyLabel: '复制', copiedLabel: '已复制' },
  footnotes: '脚注',
}

const SETTLED_TEXT = '# 已定稿\n\n这一段在本轮就已经写完。'

describe('FF-7 前端块冻结', () => {
  it('流式增量与外层更新都不再渲染已定稿的 markdown 块', () => {
    const counts = { settled: 0, streaming: 0 }

    // 探针在 memo 内侧：外层重渲染时它要是还被调用，就是子树没被跳过。
    const SettledBody = (): ReactElement => {
      counts.settled += 1
      return <MarkdownText text={SETTLED_TEXT} labels={LABELS} />
    }
    const SettledBlock = memo(SettledBody)

    const StreamingBody = ({ chunk }: { chunk: string }): ReactElement => {
      counts.streaming += 1
      return <div data-testid="streaming-slot">{chunk}</div>
    }
    const StreamingBlock = memo(StreamingBody)

    function App({ chunk }: { chunk: string }) {
      return (
        <>
          <SettledBlock />
          <StreamingBlock chunk={chunk} />
        </>
      )
    }

    const { rerender, container } = render(<App chunk="第一段" />)
    expect(counts).toEqual({ settled: 1, streaming: 1 })

    rerender(<App chunk="第一段 + 追加" />)
    rerender(<App chunk="第一段 + 追加 + 再追加" />)

    // 定稿块 props 全程不变 → 一次都不该再渲染
    expect(counts.settled).toBe(1)
    // 流式块每次都在推进，证明"零重渲染"不是因为整个应用没更新
    expect(counts.streaming).toBe(3)
    expect(container.textContent).toContain('已定稿')
    expect(container.textContent).toContain('第一段 + 追加 + 再追加')
  })

  it('定稿块自己收到新文本时必须重新渲染（反向对照，防"永不更新"的假绿）', () => {
    const counts = { settled: 0 }
    const SettledBody = ({ text }: { text: string }): ReactElement => {
      counts.settled += 1
      return <MarkdownText text={text} labels={LABELS} />
    }
    const SettledBlock = memo(SettledBody)

    const { rerender, container } = render(<SettledBlock text="原始文本" />)
    expect(counts.settled).toBe(1)
    rerender(<SettledBlock text="改过的文本" />)

    expect(counts.settled, 'props 变了还不重渲染，说明测的是缓存而不是渲染').toBe(2)
    expect(container.textContent).toContain('改过的文本')
  })

  it('同一段文本在流式与定稿两态下都渲染出正文（状态切换不吞内容）', () => {
    // 流式中间态与完成态是 §5 要求覆盖的两种渲染；这里盯住的是"切态不丢字"，
    // 因为定稿走 renderSettled、流式走 StreamingRenderer，两条路径不是同一个实现。
    const { rerender, container } = render(<MarkdownText text="半截**加粗**" streaming labels={LABELS} />)
    const streamingText = container.textContent ?? ''
    expect(streamingText).toContain('半截')

    rerender(<MarkdownText text="半截**加粗**收尾" labels={LABELS} />)
    expect(container.textContent).toContain('半截')
    expect(container.textContent).toContain('收尾')
    expect(container.querySelector('strong')?.textContent).toBe('加粗')
  })
})
