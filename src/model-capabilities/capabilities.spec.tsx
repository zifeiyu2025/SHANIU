// @vitest-environment jsdom
/**
 * SH-132 能力视图（宿主插件）契约测试。
 *
 * 覆盖本卡承诺的五件事：
 *  1. 走的是上游 `settings.models.provider-card` 扩展点，键控 `llm-pi-ai`（不改 vendored 组件）；
 *  2. 值 / 来源 / 三态 / 内置表数据日期同屏可见，`mismatch` 有独立 data-state；
 *  3. **只标改动的那一项**：其余项的来源不被顺手改成手动；
 *  4. 设置写入广播后重新读取（否则徽标停在保存前）；
 *  5. 读取失败如实显示，不粉饰成"没有能力"；
 *  6. 密钥三态：有效 / 无效 / **无法确认**（不等于无效），保存密钥后自动重校验且不缓存。
 */
import { describe, expect, it, vi } from 'vitest'
import type { ReactNode } from 'react'
import { act, render, screen, waitFor } from '@testing-library/react'
import { Context } from '@deepseek-ai/cordis'
import { apply as capabilitiesApply, inject as capabilitiesInject } from './index'
import { CapabilityPanel } from './CapabilityPanel'
import { zh } from './locales'
import type { CapabilitiesWire, CapabilityProviderWire, KeyCheckWire } from './wire'

/** 词典直译（测试只关心键是否被用到）。 */
function translate(key: string, params?: Record<string, unknown>): string {
  const template = (zh as Record<string, string>)[key] ?? key
  return template.replace(/\{(\w+)\}/g, (_match, name: string) => String(params?.[name] ?? `{${name}}`))
}

/** 内置表数据日期（UTC 2026-11-09）。 */
const DATA_DATE = Date.UTC(2026, 10, 9)

const FLASH: CapabilityProviderWire = {
  id: 'deepseek',
  label: 'DeepSeek',
  models: [{
    id: 'deepseek-flash',
    name: 'DeepSeek Flash',
    contextWindow: 393_216,
    maxTokens: 32_768,
    input: ['text', 'image'],
    capabilities: {
      vision: { value: true, source: 'builtin', state: 'verified', evidence: '红图→红色；绿图→深绿色', catalogUpdatedAt: DATA_DATE },
      tools: { value: true, source: 'builtin', state: 'verified', catalogUpdatedAt: DATA_DATE },
      reasoning: { levels: ['low', 'high', 'max'], verifiedLevels: ['low', 'high', 'max'], source: 'builtin', state: 'verified', catalogUpdatedAt: DATA_DATE },
      context_window: { value: 393_216, source: 'builtin', state: 'unverified', catalogUpdatedAt: DATA_DATE },
      max_output: { value: 32_768, source: 'builtin', state: 'unverified', catalogUpdatedAt: DATA_DATE },
      builtin_web_search: { value: false, source: 'unknown', state: 'unverified' },
    },
  }],
}

/** 纯文本模型声明视觉：只有视觉一项是手动 + 不符，其余仍是内置表。 */
const V4_PRO: CapabilityProviderWire = {
  id: 'deepseek',
  label: 'DeepSeek',
  models: [{
    id: 'deepseek-v4-pro',
    name: 'DeepSeek V4 Pro',
    input: ['text', 'image'],
    capabilities: {
      vision: { value: true, source: 'manual', state: 'mismatch', evidence: '红图→无法识别；绿图→无法识别' },
      tools: { value: true, source: 'builtin', state: 'verified', catalogUpdatedAt: DATA_DATE },
      reasoning: { levels: ['low', 'high', 'max'], source: 'builtin', state: 'unverified', catalogUpdatedAt: DATA_DATE },
      context_window: { value: 131_072, source: 'builtin', state: 'unverified', catalogUpdatedAt: DATA_DATE },
    },
  }],
}

function panelElement(
  providers: readonly CapabilityProviderWire[],
  options: { readonly subscribe?: (handler: (event?: string) => void) => () => void } = {},
): ReactNode {
  const Panel = CapabilityPanel as unknown as (props: Record<string, unknown>) => ReactNode
  const wire: CapabilitiesWire = { providers: async () => providers }
  return (
    <Panel
      t={translate}
      wire={wire}
      subscribe={options.subscribe ?? (() => () => {})}
      provider={{ provider: 'deepseek', displayName: 'DeepSeek' }}
    />
  )
}

describe('src/model-capabilities：插件接缝', () => {
  it('占用 settings.models.provider-card，键控 llm-pi-ai，不碰模型行', () => {
    expect(capabilitiesInject).toEqual(['slots', 'locale', 'remote'])

    const waited: string[] = []
    const registrations: { key?: string }[] = []
    const ctx = new Context()
    ctx.provide('locale', { register: () => () => {}, bind: () => translate } as never)
    ctx.provide('remote', { $on: () => () => {} } as never)
    ctx.provide('slots', {
      inject: (key: string, contribution: () => void) => {
        waited.push(key)
        contribution()
        return () => {}
      },
      register: (options: { key?: string }) => { registrations.push(options); return () => {} },
    } as never)

    capabilitiesApply(ctx)
    expect(waited).toEqual(['settings.models.provider-card'])
    expect(registrations).toHaveLength(1)
    expect(registrations[0]?.key).toBe('llm-pi-ai')
  })
})

describe('src/model-capabilities：值 + 来源 + 三态', () => {
  it('内置表来源带数据日期，值与档位按视图原样显示', async () => {
    render(panelElement([FLASH]))
    await screen.findByText('DeepSeek Flash')
    expect(screen.getAllByText(zh['source.builtin']).length).toBeGreaterThan(0)
    expect(screen.getAllByText(zh['state.verified']).length).toBeGreaterThan(0)
    expect(screen.getByText(/2026-11-09/)).toBeTruthy()
    expect(screen.getByText('393,216')).toBeTruthy()
    expect(screen.getByText('low/high/max')).toBeTruthy()
  })

  it('不符项有独立 data-state 与证据，未验证项不冒充"测过不行"', async () => {
    render(panelElement([V4_PRO]))
    await screen.findByText('DeepSeek V4 Pro')
    const mismatch = document.querySelector('.shaniu-capabilities-chip[data-state="mismatch"]')
    expect(mismatch).toBeTruthy()
    expect(mismatch?.textContent).toContain(zh['state.mismatch'])
    expect(mismatch?.getAttribute('title')).toContain('红图→无法识别')
    expect(document.querySelectorAll('.shaniu-capabilities-chip[data-state="unverified"]').length).toBeGreaterThan(0)
  })

  it('手改一项后只有那一项是手动，其余来源不变', async () => {
    render(panelElement([V4_PRO]))
    await screen.findByText('DeepSeek V4 Pro')
    const manual = document.querySelectorAll('.shaniu-capabilities-source[data-source="manual"]')
    expect(manual).toHaveLength(1)
    expect(manual[0]?.closest('.shaniu-capabilities-chip')?.getAttribute('data-item')).toBe('vision')
    expect(document.querySelectorAll('.shaniu-capabilities-source[data-source="builtin"]').length).toBe(3)
  })

  it('检测中：显示「检测中」而非「未验证」，轮询到结论后自行更新', async () => {
    vi.useFakeTimers()
    try {
      const PROBING: CapabilityProviderWire = {
        id: 'deepseek',
        label: 'DeepSeek',
        models: [{
          id: 'deepseek-flash',
          name: 'DeepSeek Flash',
          capabilities: {
            vision: { value: true, source: 'manual', state: 'probing' },
            tools: { value: true, source: 'endpoint', state: 'probing' },
          },
        }],
      }
      let calls = 0
      const Panel = CapabilityPanel as unknown as (props: Record<string, unknown>) => ReactNode
      render(
        <Panel
          t={translate}
          wire={{ providers: async () => { calls += 1; return calls === 1 ? [PROBING] : [FLASH] } }}
          subscribe={() => () => {}}
          provider={{ provider: 'deepseek', displayName: 'DeepSeek' }}
        />,
      )
      await act(async () => { await vi.advanceTimersByTimeAsync(0) })
      const chip = document.querySelector('.shaniu-capabilities-chip[data-state="probing"]')
      expect(chip?.textContent).toContain(zh['state.probing'])
      // 成本量级必须在界面上，不能被当成免费操作。
      expect(screen.getByText(zh.cost)).toBeTruthy()
      // 「检测中」必须看得出在动：要有已耗时计数与预计时长，不能只显示一个静态词。
      expect(screen.getByText(/预计 10–20 秒/)).toBeTruthy()

      await act(async () => { await vi.advanceTimersByTimeAsync(2_000) })
      expect(document.querySelector('.shaniu-capabilities-chip[data-state="probing"]')).toBeNull()
      expect(document.querySelector('.shaniu-capabilities-chip[data-state="verified"]')).toBeTruthy()
      expect(screen.queryByText(/预计 10–20 秒/)).toBeNull()
    } finally {
      vi.useRealTimers()
    }
  })

  it('不可判：独立状态、说明文案与手动重探入口', async () => {
    const UNJUDGEABLE: CapabilityProviderWire = {
      id: 'deepseek',
      label: 'DeepSeek',
      models: [{
        id: 'deepseek-flash',
        name: 'DeepSeek Flash',
        capabilities: {
          vision: { value: true, source: 'builtin', state: 'verified', catalogUpdatedAt: DATA_DATE },
          tools: { value: true, source: 'builtin', state: 'verified', catalogUpdatedAt: DATA_DATE },
          reasoning: {
            levels: ['low', 'high', 'max'],
            source: 'builtin',
            state: 'unjudgeable',
            evidence: '效应/噪声 0.8 低于门槛 3——数据不足，无法判定',
            catalogUpdatedAt: DATA_DATE,
          },
        },
      }],
    }
    const reprobe = vi.fn(async () => {})
    const Panel = CapabilityPanel as unknown as (props: Record<string, unknown>) => ReactNode
    render(
      <Panel
        t={translate}
        wire={{ providers: async () => [UNJUDGEABLE], reprobe }}
        subscribe={() => () => {}}
        provider={{ provider: 'deepseek', displayName: 'DeepSeek' }}
      />,
    )
    await screen.findByText('DeepSeek Flash')
    const chip = document.querySelector('.shaniu-capabilities-chip[data-item="reasoning"]')
    // 「不可判」不能显示成「未验证」：前者是测过之后的结论，后者是没测成。
    expect(chip?.getAttribute('data-state')).toBe('unjudgeable')
    expect(chip?.textContent).toContain(zh['state.unjudgeable'])
    expect(chip?.getAttribute('title')).toContain('门槛 3')
    // 文案要告诉用户"不影响使用"，避免被读成能力不可用。
    expect(screen.getByText(zh.unjudgeableHint)).toBeTruthy()
    // 终态只能手动打破：按钮存在，点击走 force 重探。
    const button = screen.getByRole('button', { name: zh.retry })
    await act(async () => { button.click() })
    expect(reprobe).toHaveBeenCalledWith('deepseek', 'deepseek-flash')
  })

  it('读取失败如实显示，不显示成"没有能力"', async () => {
    const Panel = CapabilityPanel as unknown as (props: Record<string, unknown>) => ReactNode
    render(
      <Panel
        t={translate}
        wire={{ providers: async () => { throw new Error('get_settings unavailable') } }}
        subscribe={() => () => {}}
        provider={{ provider: 'deepseek', displayName: 'DeepSeek' }}
      />,
    )
    await screen.findByText(/get_settings unavailable/)
  })

  it('设置写入广播后重新读取能力视图', async () => {
    let rows: readonly CapabilityProviderWire[] = [FLASH]
    const Panel = CapabilityPanel as unknown as (props: Record<string, unknown>) => ReactNode
    let refresh: (() => void) | undefined
    const subscribe = (handler: () => void): (() => void) => {
      refresh = handler
      return () => {}
    }
    render(
      <Panel
        t={translate}
        wire={{ providers: async () => rows }}
        subscribe={subscribe}
        provider={{ provider: 'deepseek', displayName: 'DeepSeek' }}
      />,
    )
    await screen.findByText('DeepSeek Flash')
    rows = [V4_PRO]
    refresh?.()
    await waitFor(() => expect(screen.getByText('DeepSeek V4 Pro')).toBeTruthy())
  })
})

const VALID: KeyCheckWire = { state: 'valid', reason: 'ok', message: '密钥有效', latencyMs: 42 }

const INVALID: KeyCheckWire = {
  state: 'invalid',
  reason: 'key_invalid',
  message: 'API Key 无效或没有访问权限',
  httpStatus: 401,
  providerCode: 'invalid_api_key',
  providerMessage: 'Incorrect API key provided',
}

const UNKNOWN_NETWORK: KeyCheckWire = {
  state: 'unknown',
  reason: 'network',
  message: '无法连接到端点（网络不通或地址不对）',
}

/**
 * 密钥三态视图。`providers` 默认返回空清单：只有 Key、没有模型时三态也必须可见
 * （这正是"提前 return 空模型"会让最需要的结论消失的那个洞）。
 */
function keyPanel(options: {
  readonly checkKey: () => Promise<KeyCheckWire>
  readonly keyConfigured?: boolean
  readonly subscribe?: (handler: (event?: string) => void) => () => void
}): ReactNode {
  const Panel = CapabilityPanel as unknown as (props: Record<string, unknown>) => ReactNode
  return (
    <Panel
      t={translate}
      wire={{ providers: async () => [], checkKey: options.checkKey }}
      subscribe={options.subscribe ?? (() => () => {})}
      keyConfigured={options.keyConfigured ?? true}
      provider={{ provider: 'deepseek', displayName: 'DeepSeek' }}
    />
  )
}

function keyState(): string | null | undefined {
  return document.querySelector('.shaniu-key')?.getAttribute('data-state')
}

describe('src/model-capabilities：密钥三态（A0-1）', () => {
  it('没有模型清单时三态照样显示；未保存密钥显示「未验证」且不发请求', async () => {
    const checkKey = vi.fn(async () => VALID)
    render(keyPanel({ checkKey, keyConfigured: false }))
    await screen.findByText(zh['key.title'])
    expect(keyState()).toBe('unverified')
    expect(screen.getByText(zh['key.unverified'])).toBeTruthy()
    expect(screen.getByText(zh['key.notConfigured'])).toBeTruthy()
    expect(checkKey).not.toHaveBeenCalled()
  })

  it('校验中 → 有效：过程中能看见「校验中」，结论到了才换成「密钥有效」', async () => {
    let settle: (value: KeyCheckWire) => void = () => {}
    const pending = new Promise<KeyCheckWire>((resolve) => { settle = resolve })
    const checkKey = vi.fn(() => pending)
    render(keyPanel({ checkKey }))
    await screen.findByText(zh['key.checking'])
    expect(keyState()).toBe('checking')

    await act(async () => { settle(VALID) })
    await screen.findByText(zh['key.valid'])
    expect(keyState()).toBe('valid')
    expect(screen.getByText(zh['key.reason.ok'])).toBeTruthy()
    expect(screen.getByText('42 毫秒')).toBeTruthy()
  })

  it('「无法确认」是中性态：不是失败色，也不会被写成「密钥无效」', async () => {
    render(keyPanel({ checkKey: async () => UNKNOWN_NETWORK }))
    await screen.findByText(zh['key.unknown'])
    // 独立的 data-state：视觉由 CSS 按它分档，unknown 既不是 success 也不是 error。
    expect(keyState()).toBe('unknown')
    expect(document.querySelector('[data-state="invalid"]')).toBeNull()
    expect(screen.queryByText(zh['key.invalid'])).toBeNull()
    expect(screen.queryByText(zh['key.valid'])).toBeNull()
    // 原因必须是"网络不通"这类，用户才知道该修网络而不是换一把正确的 Key。
    expect(screen.getByText(zh['key.reason.network'])).toBeTruthy()
  })

  it('无效时给出原因、HTTP 状态与可展开复制的原始响应', async () => {
    render(keyPanel({ checkKey: async () => INVALID }))
    await screen.findByText(zh['key.invalid'])
    expect(keyState()).toBe('invalid')
    expect(screen.getByText(zh['key.reason.key_invalid'])).toBeTruthy()
    expect(screen.getByText('HTTP 401')).toBeTruthy()

    const raw = document.querySelector('.shaniu-key-raw pre')?.textContent ?? ''
    expect(raw).toContain('invalid_api_key')
    expect(raw).toContain('Incorrect API key provided')

    const writeText = vi.fn(async (_text: string) => {})
    Object.defineProperty(navigator, 'clipboard', { value: { writeText }, configurable: true })
    const copy = screen.getByRole('button', { name: zh['key.copy'] })
    await act(async () => { copy.click() })
    expect(writeText).toHaveBeenCalledWith(raw)
    await screen.findByText(zh['key.copied'])
  })

  it('保存密钥后自动重校验：旧结论立刻让位，新结论到了才显示，不吃缓存', async () => {
    let settle: (value: KeyCheckWire) => void = () => {}
    const checkKey = vi.fn()
      .mockResolvedValueOnce(INVALID)
      .mockImplementationOnce(() => new Promise<KeyCheckWire>((resolve) => { settle = resolve }))
    let notify: ((event?: string) => void) | undefined
    const subscribe = (handler: (event?: string) => void): (() => void) => {
      notify = handler
      return () => {}
    }
    render(keyPanel({ checkKey, subscribe }))
    await screen.findByText(zh['key.invalid'])
    expect(checkKey).toHaveBeenCalledTimes(1)

    // `credentials/reference-updated` 是 `credentials.set` 成功后的广播：必须重新问端点。
    await act(async () => { notify?.('credentials/reference-updated') })
    expect(checkKey).toHaveBeenCalledTimes(2)
    // 重校验期间必须回到「校验中」：把旧结论挂在界面上就是在 UI 层缓存一个
    // 可能已经失效的判断（换了 Key 仍显示"无效"，用户会以为新 Key 也不行）。
    expect(keyState()).toBe('checking')
    expect(screen.queryByText(zh['key.invalid'])).toBeNull()

    await act(async () => { settle(VALID) })
    await screen.findByText(zh['key.valid'])
    expect(screen.queryByText(zh['key.invalid'])).toBeNull()
  })

  it('「重新校验」按钮重新问一次端点', async () => {
    const checkKey = vi.fn(async () => VALID)
    render(keyPanel({ checkKey }))
    await screen.findByText(zh['key.valid'])
    expect(checkKey).toHaveBeenCalledTimes(1)

    const button = screen.getByRole('button', { name: zh['key.recheck'] })
    await act(async () => { button.click() })
    await waitFor(() => expect(checkKey).toHaveBeenCalledTimes(2))
  })

  it('校验命令自身失败时归「无法确认」，不冒充「密钥无效」', async () => {
    render(keyPanel({
      checkKey: async () => { throw new Error('ipc unavailable') },
    }))
    await screen.findByText(zh['key.unknown'])
    expect(keyState()).toBe('unknown')
    expect(screen.getByText(/ipc unavailable/)).toBeTruthy()
    expect(screen.queryByText(zh['key.invalid'])).toBeNull()
  })
})
