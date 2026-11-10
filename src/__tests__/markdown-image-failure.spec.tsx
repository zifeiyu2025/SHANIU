import { fireEvent, render, screen } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { MarkdownDelegateProvider } from '../dsh/packages/client/ui-primitives/src/markdown/MarkdownDelegate'
import { MarkdownText } from '../dsh/packages/client/ui-primitives/src/markdown/MarkdownText'

/**
 * SH-137：失败的图片不得渲染成裸 `<a href>`（Tauri 会把整个应用导航到外网）；
 * 绝对 http(s) 目标渲染成按钮，点击走全局 `window.open`——桌面装配期那个全局
 * 已被换成宿主垫片（`src/external-link/route.ts` → `open_external_url`）。
 */

const LABELS = { code: { copyLabel: '复制', copiedLabel: '已复制' }, footnotes: '脚注' }

const IMAGE_LABELS = {
  open: '查看大图', loading: '加载中…', failed: '图片无法预览', openExternal: '在浏览器打开',
  dialog: '图片预览', close: '关闭',
}

function renderImage(
  markdown: string,
  options: { resolve?: (path: string) => string | undefined } = {},
): HTMLElement {
  const { container } = render(
    <MarkdownDelegateProvider
      openFile={vi.fn()}
      fileImages={{
        fileDrag: { start: vi.fn(), end: vi.fn() } as never,
        resolve: options.resolve ?? (() => undefined),
        labels: IMAGE_LABELS,
      }}
    >
      <MarkdownText text={markdown} labels={LABELS} />
    </MarkdownDelegateProvider>,
  )
  return container
}

afterEach(() => {
  vi.unstubAllGlobals()
})

describe('Markdown 失败图片（SH-137）', () => {
  it('绝对 http(s) 目标：失败态是可聚焦按钮，点击走全局 window.open（宿主垫片）', () => {
    const opened = vi.fn(() => null)
    vi.stubGlobal('open', opened)
    const container = renderImage('![示意图](https://example.com/missing.png)')
    fireEvent.error(container.querySelector('img') as HTMLImageElement)

    expect(container.querySelector('a')).toBeNull()
    const button = screen.getByRole('button', { name: /图片无法预览.*示意图.*在浏览器打开/ })
    expect(button.getAttribute('type')).toBe('button')
    fireEvent.click(button)
    expect(opened).toHaveBeenCalledWith('https://example.com/missing.png', '_blank', 'noopener,noreferrer')
  })

  it('相对路径目标：即使显示源是宿主媒体路由，失败态也保持纯文本（不渲染链接）', () => {
    const opened = vi.fn(() => null)
    vi.stubGlobal('open', opened)
    const container = renderImage('![示意图](missing.png)', {
      resolve: (path) => `http://tauri.localhost/api/file?path=${path}`,
    })
    fireEvent.error(container.querySelector('img') as HTMLImageElement)

    expect(screen.queryByRole('button')).toBeNull()
    expect(container.querySelector('a')).toBeNull()
    expect(container.textContent).toContain('图片无法预览')
    expect(opened).not.toHaveBeenCalled()
  })
})
