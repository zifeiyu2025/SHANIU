/**
 * SH-113：搜索的失败态与残缺态。
 *
 * 为什么落在 SHANIU 面：上游 `src/dsh/**` 的 spec 依赖未 vendored 的
 * `@deepseek-ai/dsh-client-test-runtime`，根 vitest 也把 vendored 树排除在验收面外，
 * 放那里等于永远不跑（docs/05 SH-101 的既有先例）。
 *
 * 断言两件用户能看见的事：
 * ① 搜索失败不能渲染成「无匹配会话」——必须显示失败原因和「重试」入口；
 * ② `skippedSessions > 0` 时必须说清「这次结果不完整」，不能悄悄少给。
 */
// @vitest-environment jsdom
import { fireEvent, render, screen } from '@testing-library/react'
import { describe, expect, it, vi } from 'vitest'
import { zh } from '../dsh/packages/client/ui-workspace/src/client/locales.ts'
import {
  SearchResults,
} from '../dsh/packages/client/ui-workspace/src/client/rows/WorkspaceBrowser.tsx'

/** `t` 的最小实现：取中文词典，并按 `{name}` 插值（与上游 `register` 的语义一致）。 */
function translate(
  key: string,
  vars?: Record<string, string | number>,
): string {
  const template = (zh as Record<string, string>)[key] ?? key
  return template.replace(/\{(\w+)\}/g, (_, name: string) => String(vars?.[name] ?? ''))
}

function mountSearch(remote: {
  status: 'idle' | 'loading' | 'ready' | 'error'
  skippedSessions: number
  error: string
}) {
  const onRetry = vi.fn()
  render(
    <SearchResults
      useSessions={((selector: (state: unknown) => unknown) =>
        selector({ ids: [], byId: {}, mainId: null })) as never}
      useSessionStatus={((selector: (state: unknown) => unknown) =>
        selector(new Map())) as never}
      usePanelInfo={((selector: (state: unknown) => unknown) =>
        selector({ activePanelId: null })) as never}
      useSessionAccess={(() => ({ allowed: true, reason: '' })) as never}
      open={() => {}}
      onUnarchive={() => {}}
      workspaces={[]}
      archivedSessionIds={[]}
      archivedFilter="default"
      query="needle"
      remote={{
        query: 'needle',
        status: remote.status,
        items: [],
        hasMore: false,
        skippedSessions: remote.skippedSessions,
        error: remote.error,
      }}
      resultLimit={20}
      t={translate as never}
      onRetry={onRetry}
    />,
  )
  return { onRetry }
}

describe('workspace search states', () => {
  it('shows the failure reason and a retry control instead of "no matches"', () => {
    const { onRetry } = mountSearch({ status: 'error', skippedSessions: 0, error: '索引仍不可用' })

    const alert = screen.getByRole('alert')
    expect(alert.textContent).toContain(translate('search.failed'))
    expect(alert.textContent).toContain('索引仍不可用')
    expect(screen.queryByText(translate('search.noMatches'))).toBeNull()

    fireEvent.click(screen.getByRole('button', { name: translate('search.retry') }))
    expect(onRetry).toHaveBeenCalledOnce()
  })

  it('states that skipped archives make the page incomplete', () => {
    mountSearch({ status: 'ready', skippedSessions: 2, error: '' })

    expect(screen.getByText(translate('search.skipped', { n: 2 }))).toBeTruthy()
  })
})
