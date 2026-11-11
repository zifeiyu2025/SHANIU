/**
 * 设置页服务商卡片里的能力区块（宿主插件，非 vendored）。
 *
 * 为什么占 `settings.models.provider-card`：上游 slot-contract 明确把这个槽位定义为
 * "外部插件往模型设置页加 UI 的座位"（按 `settingsNs` 键控）。上游模型行本身只编辑
 * 值（contextWindow / maxTokens / 输入模态 / 推理档），**不显示"谁说的、测过没有"**；
 * 这两样正是 SH-132 要补的，所以走扩展点，不改 vendored 组件——否则每次跟上游同步都会丢。
 *
 * 展示口径：
 *  - 值来自视图（`capability::model_view`），不在这里重新推导；
 *  - 来源徽标（内置表/端点/手动/推测/实测）与实测三态并列，`mismatch` 必须显眼；
 *  - 内置表来源旁显示数据日期，表过时要看得出来；
 *  - 实测证据放 `title`，不折叠成"通过"两个字。
 *
 * A0-1 的密钥三态（`check_api_key`）也挂在这里：密钥编辑框就在同一张卡片上，
 * 校验结果离输入框越近越不容易被忽略。三态的口径见文件末尾 `KeyState`。
 */
import { useEffect, useState, type ReactNode } from 'react'
import type { PropsLocale, PropsRuntime } from '@deepseek-ai/dsh-client-ui-slots'
import type {} from '@deepseek-ai/dsh-client-ui-settings-models/client'
import { Button } from '@deepseek-ai/dsh-client-ui-primitives'
import { writeClipboardText } from '../host/clipboard'
import { NS, type CapabilitiesKey } from './locales'
import type { CapabilitiesWire, CapabilityModelWire, CapabilityViewWire, KeyCheckWire } from './wire'
import './capabilities.css'

/**
 * 子集：与宿主 `capability::ITEMS` 的 id 一一对应（新增能力项时两处一起加）。
 *
 * 注意：`builtin_web_search` 刻意不在列表里——端点托管搜索尚未接线（实际搜索永远走
 * 本地 Bing 抓页），显示「托管搜索可用」会误导用户。接线后与宿主表一起加回。
 */
const ITEMS = ['vision', 'tools', 'reasoning', 'context_window', 'max_output'] as const
type ItemId = typeof ITEMS[number]

const ITEM_LABEL: Record<ItemId, CapabilitiesKey> = {
  vision: 'item.vision',
  tools: 'item.tools',
  reasoning: 'item.reasoning',
  context_window: 'item.context_window',
  max_output: 'item.max_output',
}

const SOURCE_LABEL: Record<string, CapabilitiesKey> = {
  unknown: 'source.unknown',
  endpoint: 'source.endpoint',
  builtin: 'source.builtin',
  inferred: 'source.inferred',
  probe: 'source.probe',
  manual: 'source.manual',
}

const STATE_LABEL: Record<string, CapabilitiesKey> = {
  verified: 'state.verified',
  mismatch: 'state.mismatch',
  probing: 'state.probing',
  unverified: 'state.unverified',
  unjudgeable: 'state.unjudgeable',
}

/**
 * 密钥校验的界面状态。与后端 `KeyCheckState` **不是**一一对应：
 *  - `unverified`：还没结论（没保存密钥，或后端明确说没读到）；
 *  - `checking`：请求在飞（含重新校验）；
 *  - `valid` / `invalid`：端点给出了结论；
 *  - `unknown`：**无法确认**——网络不通、超时、限流、端点不支持等。
 *
 * `unknown` 既不是失败也不是成功。把它画成失败色，用户会去换掉一把本来正确的
 * Key；画成成功色，配错会被当成配好。这是三态存在的全部理由。
 */
type KeyState = 'unverified' | 'checking' | 'valid' | 'invalid' | 'unknown'

const KEY_STATE_LABEL: Record<KeyState, CapabilitiesKey> = {
  unverified: 'key.unverified',
  checking: 'key.checking',
  valid: 'key.valid',
  invalid: 'key.invalid',
  unknown: 'key.unknown',
}

/** 后端稳定原因码 → 人话（未知码回落到后端自带的 `message`）。 */
const REASON_LABEL: Record<string, CapabilitiesKey> = {
  ok: 'key.reason.ok',
  key_invalid: 'key.reason.key_invalid',
  missing: 'key.reason.missing',
  no_provider: 'key.reason.no_provider',
  no_endpoint: 'key.reason.no_endpoint',
  storage: 'key.reason.storage',
  network: 'key.reason.network',
  timeout: 'key.reason.timeout',
  endpoint_unsupported: 'key.reason.endpoint_unsupported',
  rate_limited: 'key.reason.rate_limited',
  server: 'key.reason.server',
  quota: 'key.reason.quota',
  bad_request: 'key.reason.bad_request',
  unknown: 'key.reason.unknown',
}

/** 「检测中」的轮询间隔：探针本身有 30s 总超时，2s 一取不会打爆宿主命令。 */
const PROBING_POLL_MS = 2_000

/** 「已复制」停留时长。 */
const COPIED_MS = 2_000

type Props = PropsRuntime<'settings.models.provider-card'> & PropsLocale<typeof NS> & {
  readonly wire: CapabilitiesWire
  /**
   * 设置写入广播。事件名用于区分「文档变了」（能力来源可能变，要重读）与
   * 「密钥/引用变了」（要重新校验），没有事件名时两件事都做。
   */
  readonly subscribe: (handler: (event?: string) => void) => () => void
}

/** 千分位：上下文窗口这类数字要能一眼读出量级。 */
function formatCount(value: number): string {
  return String(value).replace(/\B(?=(\d{3})+(?!\d))/g, ',')
}

/** 内置表数据日期（毫秒 → YYYY-MM-DD）；缺省/0 视为没有。 */
function dateOf(ms: number | undefined): string | undefined {
  if (ms === undefined || ms <= 0) return undefined
  return new Date(ms).toISOString().slice(0, 10)
}

function valueText(
  item: ItemId,
  view: CapabilityViewWire,
  t: (key: CapabilitiesKey, params?: Record<string, unknown>) => string,
): string {
  if (item === 'reasoning') {
    const levels = view.verifiedLevels?.length ? view.verifiedLevels : view.levels ?? []
    return levels.length > 0 ? levels.join('/') : t('value.no')
  }
  if (item === 'context_window' || item === 'max_output') {
    return typeof view.value === 'number' && view.value > 0 ? formatCount(view.value) : t('value.none')
  }
  if (typeof view.value === 'boolean') return view.value ? t('value.yes') : t('value.no')
  // 没声明时的值与"声明了没有"是两件事：写"无"会被读成"明确没有"。
  return '—'
}

export function CapabilityPanel(props: Props): ReactNode {
  const { provider, t, wire, subscribe, keyConfigured } = props
  const [models, setModels] = useState<readonly CapabilityModelWire[]>()
  const [error, setError] = useState<string>()
  const [tick, setTick] = useState(0)

  // 密钥校验的状态。刻意**不做缓存**：挂载、保存密钥、改端点都重新问端点。
  // 缓存会把旧结论锁在界面上——换了 Key 显示"无效"、修好网络显示"网络不通"。
  const keyCheck = wire.checkKey
  const isKeyRow = provider.nativeAuth === undefined && keyCheck !== undefined
  const [keyResult, setKeyResult] = useState<KeyCheckWire>()
  const [keyChecking, setKeyChecking] = useState(false)
  const [keyError, setKeyError] = useState<string>()
  const [keyNonce, setKeyNonce] = useState(0)
  const [copied, setCopied] = useState(false)

  useEffect(() => {
    let alive = true
    const load = async (): Promise<void> => {
      try {
        const providers = await wire.providers()
        if (!alive) return
        const row = providers.find(item => item.id === provider.provider)
          ?? providers.find(item => item.label !== undefined && item.label === provider.displayName)
        setModels(row?.models ?? [])
        setError(undefined)
      } catch (reason) {
        if (!alive) return
        // 读不到就如实说，不显示成"没有能力"——那是把失败伪装成结论。
        setError(reason instanceof Error ? reason.message : String(reason))
      }
    }
    void load()
    const off = subscribe((event) => {
      // 能力来源（手动/内置表）随设置文档变；密钥与端点变更可能只发密钥事件。
      if (event === undefined || event === 'settings/document-updated') void load()
      setKeyNonce(value => value + 1)
    })
    return () => {
      alive = false
      off()
    }
  }, [wire, subscribe, provider.provider, provider.displayName, tick])

  // 自动校验：首次挂载 + 每次密钥/端点广播（`keyNonce`）。没有保存密钥时不发请求，
  // 直接显示「未验证」——让后端回一个 missing 只是多一次无意义的 IPC。
  useEffect(() => {
    if (!isKeyRow || !keyConfigured || keyCheck === undefined) return
    let alive = true
    setKeyChecking(true)
    // 重校验期间旧结论必须让位：保留它等于在界面上缓存一个可能已失效的判断。
    setKeyResult(undefined)
    setKeyError(undefined)
    void (async () => {
      try {
        const result = await keyCheck(provider.provider)
        if (alive) setKeyResult(result)
      } catch (reason) {
        // 命令本身失败（IPC/装配）也只能算"无法确认"，不能写成"密钥无效"。
        if (alive) setKeyError(reason instanceof Error ? reason.message : String(reason))
      } finally {
        if (alive) setKeyChecking(false)
      }
    })()
    return () => { alive = false }
  }, [isKeyRow, keyConfigured, keyCheck, provider.provider, keyNonce])

  // 「已复制」不能一直挂着：2 秒后回落到「复制」。
  useEffect(() => {
    if (!copied) return
    const timer = setTimeout(() => setCopied(false), COPIED_MS)
    return () => clearTimeout(timer)
  }, [copied])

  // 探针在后台跑，宿主不会为"测完了"发广播（那会把 UI 绑到后台任务上），
  // 所以只要还有项在「检测中」，就自己轮询到结论回来为止。
  const probing = models?.some(model =>
    Object.values(model.capabilities ?? {}).some(view => view.state === 'probing'),
  ) ?? false
  useEffect(() => {
    if (!probing) return
    const timer = setTimeout(() => setTick(value => value + 1), PROBING_POLL_MS)
    return () => clearTimeout(timer)
  }, [probing, models, tick])

  // 「检测中」必须看得出在动：一个静态文字会让人以为卡死。这里给已耗时计数，
  // 1s 一跳；再配合 CSS 的脉冲样式，两种信号都能说明它还活着。
  const [elapsed, setElapsed] = useState(0)
  useEffect(() => {
    if (!probing) {
      setElapsed(0)
      return
    }
    const startedAt = Date.now()
    setElapsed(0)
    const timer = setInterval(() => setElapsed(Math.round((Date.now() - startedAt) / 1000)), 1_000)
    return () => clearInterval(timer)
  }, [probing])

  // 手动重探：「不可判」是终态，只在这里（以及换 key/端点）才重测。
  const reprobe = async (model: string): Promise<void> => {
    await wire.reprobe?.(provider.provider, model)
    setTick(value => value + 1)
  }

  // 结论派生。`keyConfigured` 变 false 时旧结论必须立刻失效，不能继续显示"有效"。
  const result = keyConfigured === true ? keyResult : undefined
  const failure = keyConfigured === true ? keyError : undefined
  const checking = keyConfigured === true && keyChecking
  const keyState: KeyState = checking
    ? 'checking'
    : failure !== undefined
      ? 'unknown'
      : result === undefined || result.reason === 'missing'
        ? 'unverified'
        : result.state

  let keyNote: string | undefined
  if (failure !== undefined) {
    keyNote = t('key.failed', { error: failure })
  } else if (keyConfigured !== true) {
    keyNote = t('key.notConfigured')
  } else if (result !== undefined) {
    const label = REASON_LABEL[result.reason]
    // 未知原因码回落到后端那句人话，不吞掉它。
    keyNote = label !== undefined ? t(label) : result.message
  }

  const raw = result !== undefined ? JSON.stringify(result, null, 2) : undefined
  const copyRaw = async (text: string): Promise<void> => {
    try {
      await writeClipboardText(text)
      setCopied(true)
    } catch {
      // 剪贴板不可用时保持「复制」，不谎报「已复制」。
      setCopied(false)
    }
  }

  const modelList = models ?? []
  const hasModels = modelList.length > 0
  if (error !== undefined) {
    return <p className="shaniu-capabilities-error" role="status">{t('loadFailed', { error })}</p>
  }
  // 只有 Key、没有模型清单时也要显示三态——否则用户在最需要的地方看不到结论。
  if (!hasModels && !isKeyRow) return null

  return (
    <section className="shaniu-capabilities" aria-label={hasModels ? t('title') : undefined}>
      {isKeyRow
        ? (
          <div className="shaniu-key" data-state={keyState}>
            <div className="shaniu-key-head">
              <span className="shaniu-key-title">{t('key.title')}</span>
              <span className="shaniu-key-state" role="status">{t(KEY_STATE_LABEL[keyState])}</span>
              {result?.httpStatus !== undefined
                ? <span className="shaniu-key-meta">{t('key.http', { status: result.httpStatus })}</span>
                : null}
              {result?.latencyMs !== undefined
                ? <span className="shaniu-key-meta">{t('key.latency', { ms: result.latencyMs })}</span>
                : null}
              <Button
                size="sm"
                variant="ghost"
                disabled={checking || keyConfigured !== true}
                onClick={() => setKeyNonce(value => value + 1)}
              >
                {t('key.recheck')}
              </Button>
            </div>
            {keyNote !== undefined ? <p className="shaniu-key-note">{keyNote}</p> : null}
            {raw !== undefined
              ? (
                <details className="shaniu-key-raw">
                  <summary>{t('key.details')}</summary>
                  <pre>{raw}</pre>
                  <Button size="sm" variant="ghost" onClick={() => { void copyRaw(raw) }}>
                    {copied ? t('key.copied') : t('key.copy')}
                  </Button>
                </details>
              )
              : null}
          </div>
        )
        : null}
      {hasModels
        ? (
          <>
            <div className="shaniu-capabilities-head">
              <span className="shaniu-capabilities-title">{t('title')}</span>
              <span className="shaniu-capabilities-hint">{t('hint')}</span>
            </div>
            {/* 检测会真打请求、烧 token：成本量级必须写在用户看得见的地方，不能默认"免费"。 */}
            <p className="shaniu-capabilities-cost" title={t('costDetail')}>{t('cost')}</p>
            {probing
              ? <p className="shaniu-capabilities-progress" role="status">{t('probingProgress', { seconds: elapsed })}</p>
              : null}
            {modelList.map(model => {
              const capabilities = model.capabilities ?? {}
              const date = dateOf(
                ITEMS.map(item => capabilities[item]?.catalogUpdatedAt).find(value => value !== undefined),
              )
              const hasUnjudgeable = ITEMS.some(item => capabilities[item]?.state === 'unjudgeable')
              return (
                <div className="shaniu-capabilities-model" key={model.id}>
                  <div className="shaniu-capabilities-model-head">
                    <span className="shaniu-capabilities-model-name">
                      {model.name !== undefined && model.name !== '' ? model.name : model.id}
                    </span>
                    {date !== undefined
                      ? <span className="shaniu-capabilities-date">{t('dataDate', { date })}</span>
                      : null}
                    {hasUnjudgeable && wire.reprobe !== undefined
                      ? (
                        <Button
                          size="sm"
                          variant="ghost"
                          onClick={() => { void reprobe(model.id) }}
                        >
                          {t('retry')}
                        </Button>
                      )
                      : null}
                  </div>
                  <div className="shaniu-capabilities-items">
                    {ITEMS.map(item => {
                      const view = capabilities[item]
                      if (view === undefined) return null
                      const state = view.state ?? 'unverified'
                      const source = view.source ?? 'unknown'
                      return (
                        <span
                          className="shaniu-capabilities-chip"
                          data-state={state}
                          data-item={item}
                          key={item}
                          title={view.evidence !== undefined && view.evidence !== ''
                            ? t('evidence', { evidence: view.evidence })
                            : undefined}
                        >
                          <span className="shaniu-capabilities-label">{t(ITEM_LABEL[item])}</span>
                          <span className="shaniu-capabilities-value">{valueText(item, view, t)}</span>
                          <span className="shaniu-capabilities-source" data-source={source}>
                            {t(SOURCE_LABEL[source] ?? 'source.unknown')}
                          </span>
                          <span className="shaniu-capabilities-state">{t(STATE_LABEL[state] ?? 'state.unverified')}</span>
                        </span>
                      )
                    })}
                  </div>
                  {hasUnjudgeable
                    ? <p className="shaniu-capabilities-note">{t('unjudgeableHint')}</p>
                    : null}
                </div>
              )
            })}
          </>
        )
        : null}
    </section>
  )
}
