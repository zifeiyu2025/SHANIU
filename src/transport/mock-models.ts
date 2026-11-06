/**
 * 模型设置页与 composer 模型席位的唯一 mock 事实源（SH-005~008 mock 联调，仅浏览器开发态）。
 * 一份内存 store 同时喂四张面：`remote.llm` / `remote.settings` / `remote.credentials` 与
 * `session.modelCatalog`——设置页添加服务商或存密钥后，席位目录随即跟上，不留两份数据。
 * schema 信封按设置页自己的读法构造（`providers.<probe>.api` 的 union、
 * `reasoningEfforts` 的 dict 键枚举），不为此把适配器包拖进浏览器包。
 * 密钥只存内存，且只以 configured 状态跨界面（AGENTS §3）。
 */
import z from '@deepseek-ai/schemastery'
import type { JsonValue } from '@deepseek-ai/dsh-util-values'
import type { SettingsDescribeValue, SettingsNamespaceView, SettingsPathOpView } from '@deepseek-ai/dsh-settings/types'
import type { CredentialInfo } from '@deepseek-ai/dsh-credentials/types'
import type { LlmConfigurableProvider, LlmDiscoveredModel, LlmModelDiscoveryRequest, LlmProviderInfo } from '@deepseek-ai/dsh-llm/types'
import type { ModelCatalog, ModelProviderGroup, ModelReasoning, ModelSelection } from '@deepseek-ai/dsh-api-session-controller/types'
// 预览说明的确认字段与版本从设置页本身取，避免这里抄一份会漂移的字面量
import { WELCOME_NOTICE_ACK_FIELD, WELCOME_NOTICE_VERSION } from '@deepseek-ai/dsh-client-ui-settings-models/onboarding-copy'

/** mock 侧的 Remote 结果信封，与 gateway 的 RemoteResult 同形。 */
export type MockResult<T> =
  | { readonly ok: true; readonly value: T }
  | { readonly ok: false; readonly error: { readonly code: string; readonly message: string } }

const OK = <T>(value: T): MockResult<T> => ({ ok: true, value })
const FAIL = <T>(code: string, message: string): MockResult<T> => ({ ok: false, error: { code, message } })

/** 一个 namespace 的分层文档：base 是出厂层，user 是用户层，value 由二者解析。 */
interface Section {
  readonly schema: JsonValue
  readonly base: JsonValue
  user: JsonValue
  revision: number
}

const PROTOCOLS = ['openai-completions', 'openai-responses', 'anthropic-messages'] as const
const REASONING_LEVELS = ['off', 'low', 'medium', 'high', 'xhigh', 'max'] as const
/** 目录里档位的展示顺序（升级序）；profile 里声明了哪些就只列哪些。 */
const REASONING_ORDER = REASONING_LEVELS
/** 出厂 deepseek 的档位来自上游 model-info 的硬编码表，不是 profile 字段。 */
const DEEPSEEK_REASONING: ModelReasoning = {
  efforts: [
    { id: 'off', name: 'Off' },
    { id: 'low', name: 'Low' },
    { id: 'high', name: 'High' },
    { id: 'max', name: 'Max' },
  ],
  defaultEffort: 'high',
}

/** 手声明路由的模型行：id 必填，其余可缺；reasoningEfforts 是 `false | { [level]: wire }`。 */
const piAiModel = z.object({
  id: z.string().required(),
  name: z.string(),
  description: z.string(),
  contextWindow: z.number().step(1).min(1),
  maxTokens: z.number().step(1).min(1),
  reasoningEfforts: z.union([
    z.const(false),
    z.dict(z.union([z.string(), z.const(null)]), z.union(REASONING_LEVELS)),
  ]),
})

/** 自定义添加写入 llm-pi-ai 的 profile 形状（与 CustomProviderCard 的写入面一致）。 */
const piAiConfig = z.object({
  providers: z.dict(z.object({
    displayName: z.string(),
    apiKeyEnv: z.string().role('credential-ref'),
    api: z.union(PROTOCOLS),
    baseURL: z.string(),
    headers: z.dict(z.string()),
    models: z.array(piAiModel),
  })),
})

/** 出厂 DeepSeek 路由的 profile（设置页的 deepseek 编辑器布局读这几个字段）。 */
const deepseekConfig = z.object({
  apiKeyEnv: z.string().role('credential-ref').default('DEEPSEEK_API_KEY'),
  baseURL: z.string(),
  reasoningEffort: z.union(['off', 'low', 'high', 'max']),
  defaultContextWindow: z.number().step(1).min(1).default(1_000_000),
  maxTokens: z.number().step(1).min(1).default(256_000),
  models: z.array(z.object({
    id: z.string().required(),
    name: z.string(),
    description: z.string(),
    contextWindow: z.number().step(1).min(1),
    maxTokens: z.number().step(1).min(1),
  })),
})

/** 账号路由没有密钥引用，它走 provider 自身的鉴权链。 */
const accountConfig = z.object({
  baseURL: z.string(),
  reasoningEffort: z.union(['off', 'low', 'high', 'max']),
  defaultContextWindow: z.number().step(1).min(1).default(1_000_000),
  maxTokens: z.number().step(1).min(1).default(256_000),
  models: z.array(z.object({
    id: z.string().required(),
    name: z.string(),
    description: z.string(),
    contextWindow: z.number().step(1).min(1),
    maxTokens: z.number().step(1).min(1),
  })),
})

const envelope = (schema: { toJSON(): unknown }): JsonValue =>
  JSON.parse(JSON.stringify(schema.toJSON())) as JsonValue

const DEEPSEEK_MODELS: JsonValue = [
  { id: 'deepseek-v4-flash', name: 'DeepSeek-V4-Flash', description: '', contextWindow: 1_000_000, maxTokens: 65_536 },
  { id: 'deepseek-v4-pro', name: 'DeepSeek-V4-Pro', description: '', contextWindow: 1_000_000, maxTokens: 131_072 },
]

/** 席位预置的那条本地路由：id 与模型名沿用既有验收面，避免接真数据后退化。 */
const SEEDED_ROUTE = 'mock-provider'

const isRecord = (value: unknown): value is Record<string, JsonValue> =>
  typeof value === 'object' && value !== null && !Array.isArray(value)

/** 解析一层视图：对象逐键合并，数组与标量由用户层整体覆盖。 */
function resolve(base: JsonValue, user: JsonValue): JsonValue {
  if (!isRecord(base) || !isRecord(user)) return user === undefined ? base : user
  const merged: Record<string, JsonValue> = { ...base }
  for (const [key, value] of Object.entries(user)) {
    merged[key] = key in merged && isRecord(merged[key]) && isRecord(value) ? resolve(merged[key]!, value) : value
  }
  return merged
}

function at(value: JsonValue, path: readonly string[]): JsonValue | undefined {
  let current: unknown = value
  for (const key of path) {
    if (Array.isArray(current)) current = current[Number(key)]
    else if (isRecord(current)) current = current[key]
    else return undefined
    if (current === undefined) return undefined
  }
  return current as JsonValue | undefined
}

/** 按 op 写用户层：set 建中间对象，unset 删末级键。 */
function applyOps(section: JsonValue, ops: readonly SettingsPathOpView[]): JsonValue {
  let root: JsonValue = structuredClone(section)
  for (const op of ops) {
    if (op.path.length === 0) {
      if (op.op === 'set' && isRecord(op.value)) root = op.value
      else root = {}
      continue
    }
    if (!isRecord(root)) root = {}
    const parentPath = op.path.slice(0, -1)
    const key = op.path[op.path.length - 1] as string
    const parent = at(root, parentPath)
    if (parent === undefined || !isRecord(parent)) {
      // 中间层缺失时逐级补齐，与 Host 的 set 语义一致
      let cursor: Record<string, JsonValue> = root as Record<string, JsonValue>
      for (const segment of parentPath) {
        const next = cursor[segment]
        if (isRecord(next)) cursor = next
        else {
          const created: Record<string, JsonValue> = {}
          cursor[segment] = created
          cursor = created
        }
      }
      if (op.op === 'set') cursor[key] = op.value
      else Reflect.deleteProperty(cursor, key)
      continue
    }
    if (op.op === 'set') parent[key] = op.value
    else Reflect.deleteProperty(parent, key)
  }
  return root
}

/** 设置页与席位共用的那份 mock。 */
export interface ModelsMock {
  readonly settings: {
    describe(): Promise<MockResult<SettingsDescribeValue>>
    mutate(ns: string, ops: readonly SettingsPathOpView[], expectedRevision?: number): Promise<MockResult<SettingsNamespaceView>>
  }
  readonly llm: {
    listProviders(): Promise<MockResult<LlmProviderInfo[]>>
    listConfigurableProviders(): Promise<MockResult<LlmConfigurableProvider[]>>
    discoverModels(settingsNs: string, request: LlmModelDiscoveryRequest): Promise<MockResult<LlmDiscoveredModel[]>>
  }
  readonly credentials: {
    describe(refs: readonly string[]): Promise<MockResult<Record<string, CredentialInfo>>>
    set(ref: string, value: string): Promise<MockResult<void>>
    unset(ref: string): Promise<MockResult<void>>
  }
  /** 设置里「权限默认档」的当前值；新建会话按它初始化挡位。 */
  permissionDefault(): string | undefined
  readonly session: {
    modelCatalog(): Promise<MockResult<ModelCatalog>>
    selectModel(request: ModelSelection): Promise<MockResult<{ selected: ModelSelection }>>
  }
}

const CREDENTIAL_REF = /^[A-Za-z_][A-Za-z0-9_]*$/
const MAX_DESCRIBE_REFS = 64

/** mock 的推送失效出口，与 remote 的 $emit 同签名。 */
export interface ModelsMockOptions {
  emit(name: string, payload?: unknown): boolean
  /**
   * 选择变化回调。席位读的是 `modelSelection` 投影而非目录默认项，
   * 不回推的话选了推理档会在下一次读投影时回弹到默认档。
   */
  onSelectionChanged?(selection: ModelSelection): void
}

export function createModelsMock(options: ModelsMockOptions = { emit: () => false }): ModelsMock {
  const emit = (name: string, payload?: unknown): void => { options.emit(name, payload) }
  const sections = new Map<string, Section>([
    ['llm-deepseek', {
      schema: envelope(deepseekConfig),
      base: { apiKeyEnv: 'DEEPSEEK_API_KEY', baseURL: 'https://api.deepseek.com', reasoningEffort: 'high', defaultContextWindow: 1_000_000, maxTokens: 256_000, models: DEEPSEEK_MODELS },
      user: {},
      revision: 0,
    }],
    ['llm-deepseek-account', {
      schema: envelope(accountConfig),
      base: { baseURL: 'https://api.deepseek.com', reasoningEffort: 'high', defaultContextWindow: 1_000_000, maxTokens: 256_000, models: DEEPSEEK_MODELS },
      user: {},
      revision: 0,
    }],
    ['llm-pi-ai', {
      schema: envelope(piAiConfig),
      base: { providers: {} },
      user: {
        providers: {
          [SEEDED_ROUTE]: {
            displayName: 'Mock Provider',
            api: 'openai-responses',
            baseURL: 'https://mock.invalid/v1',
            apiKeyEnv: 'MOCK_PROVIDER_API_KEY',
            models: [
              {
                id: 'mock-model-a', name: 'Mock Model A', description: 'mock 旗舰模型', contextWindow: 128_000, maxTokens: 8_192,
                // 会推理的模型：档位就是目录里能选的那几行（Model B 故意不声明，用于对照"无推理档"的表现）
                reasoningEfforts: { off: null, low: 'low', medium: 'medium', high: 'high' },
              },
              { id: 'mock-model-b', name: 'Mock Model B', description: 'mock 轻量模型', contextWindow: 64_000, maxTokens: 4_096 },
            ],
          },
        },
      },
      revision: 0,
    }],
    ['ui-settings', {
      schema: envelope(z.object({ enabled: z.boolean().default(true) })),
      base: { enabled: true },
      user: {},
      revision: 0,
    }],
    ['ui-settings-general', {
      schema: envelope(z.object({ welcomeNoticeVersion: z.string() })),
      // SHANIU 不在 DSH 的预览版计划里：预置成"已确认"，首屏不再弹上游的预览说明
      base: { [WELCOME_NOTICE_ACK_FIELD]: WELCOME_NOTICE_VERSION },
      user: {},
      revision: 0,
    }],
    ['permission', {
      // 设置-通用里的「权限默认档」行读这个命名空间；新建会话按它初始化
      schema: envelope(z.object({ defaultPreset: z.string() })),
      base: { defaultPreset: 'read-only' },
      user: {},
      revision: 0,
    }],
  ])

  const credentials = new Map<string, string>([['MOCK_PROVIDER_API_KEY', 'mock-local-key']])
  let selection: ModelSelection = { provider: SEEDED_ROUTE, model: 'mock-model-a' }

  const viewOf = (ns: string): SettingsNamespaceView | undefined => {
    const section = sections.get(ns)
    if (section === undefined) return undefined
    return {
      ns,
      schema: section.schema,
      base: section.base,
      user: section.user,
      value: resolve(section.base, section.user),
      applies: 'live',
      autoGenerate: true,
      secrets: [],
      revision: section.revision,
    }
  }

  /** 用户层里已声明的手写路由（自定义添加的结果就在这里）。 */
  const declaredProfiles = (): [string, Record<string, JsonValue>][] => {
    const view = viewOf('llm-pi-ai')
    const providers = view === undefined ? undefined : at(view.value, ['providers'])
    if (!isRecord(providers)) return []
    return Object.entries(providers).filter(([, profile]) => isRecord(profile)) as [string, Record<string, JsonValue>][]
  }

  /** profile 引用的密钥是否已就位；没有引用即无需密钥。 */
  const keyReady = (ref: string | undefined): boolean => ref === undefined || credentials.has(ref)

  const profileUsable = (profile: Record<string, JsonValue>): boolean => {
    const models = profile.models
    return typeof profile.baseURL === 'string' && profile.baseURL.length > 0
      && Array.isArray(models) && models.length > 0
      && keyReady(typeof profile.apiKeyEnv === 'string' ? profile.apiKeyEnv : undefined)
  }

  /** 一个可路由的 provider 分组；目录项与席位分组由同一判定产生。 */
  const usableGroups = (): ModelProviderGroup[] => {
    const groups: ModelProviderGroup[] = []
    const account = viewOf('llm-deepseek-account')
    const accountModels = account === undefined ? undefined : at(account.value, ['models'])
    if (Array.isArray(accountModels) && accountModels.length > 0) {
      groups.push({ id: 'deepseek-account', name: 'DeepSeek Account', models: catalogModels(accountModels, DEEPSEEK_REASONING) })
    }
    const deepseek = viewOf('llm-deepseek')
    if (deepseek !== undefined && keyReady(at(deepseek.value, ['apiKeyEnv']) as string | undefined)) {
      const models = at(deepseek.value, ['models'])
      if (Array.isArray(models) && models.length > 0) groups.push({ id: 'deepseek', name: 'DeepSeek', models: catalogModels(models, DEEPSEEK_REASONING) })
    }
    for (const [provider, profile] of declaredProfiles()) {
      if (!profileUsable(profile)) continue
      const models = Array.isArray(profile.models) ? profile.models : []
      if (models.length === 0) continue
      groups.push({
        id: provider,
        name: typeof profile.displayName === 'string' && profile.displayName.length > 0 ? profile.displayName : provider,
        models: catalogModels(models),
      })
    }
    return groups
  }

  /**
   * 模型目录里的推理等级：席位在没有 reasoning 时整行不渲染，所以这里必须从 profile 的
   * `reasoningEfforts` 键推出来。出厂 deepseek 走上游 model-info 的硬编码档位（off/low/high/max），
   * 手声明路由按模型自己声明的键。档位的 wire 值只影响真实请求，目录里只需要 id 与显示名。
   */
  const reasoningOf = (model: Record<string, JsonValue>, fallback?: ModelReasoning): ModelReasoning | undefined => {
    const declared = model.reasoningEfforts
    if (!isRecord(declared)) return fallback
    const levels = REASONING_ORDER.filter(level => Object.hasOwn(declared, level))
    if (levels.length === 0) return undefined
    return {
      efforts: levels.map(level => ({ id: level, name: level[0]!.toUpperCase() + level.slice(1) })),
      defaultEffort: levels.includes('high') ? 'high' : levels[levels.length - 1]!,
    }
  }

  const catalogModels = (
    models: readonly unknown[],
    fallbackReasoning?: ModelReasoning,
  ): ModelProviderGroup['models'] =>
    models.filter(isRecord).map(model => {
      const reasoning = reasoningOf(model, fallbackReasoning)
      return {
        id: String(model.id ?? ''),
        name: String(model.name ?? model.id ?? ''),
        ...(typeof model.description === 'string' ? { description: model.description } : {}),
        ...(reasoning === undefined ? {} : { reasoning }),
      }
    }).filter(model => model.id.length > 0)

  return {
    settings: {
      describe: async () => {
        const namespaces = [...sections.keys()].flatMap(ns => {
          const view = viewOf(ns)
          return view === undefined ? [] : [view]
        })
        return OK<SettingsDescribeValue>({ writable: true, hasDocument: true, namespaces })
      },
      mutate: async (ns, ops, expectedRevision) => {
        const section = sections.get(ns)
        if (section === undefined) return FAIL<SettingsNamespaceView>('settings/rejected', `unknown settings namespace: ${ns}`)
        if (expectedRevision !== undefined && expectedRevision !== section.revision) {
          return FAIL<SettingsNamespaceView>('settings/conflict', `stale revision ${expectedRevision}, stored ${section.revision}`)
        }
        section.user = applyOps(section.user, ops)
        section.revision += 1
        const view = viewOf(ns)!
        // 与宿主一致：先报文档已变，settings 镜像据此重读；改的是路由目录时再报适配器已变
        emit('settings/document-updated', [ns, section.revision])
        if (ns === 'llm-pi-ai') emit('llm/adapters-updated')
        return OK(view)
      },
    },
    llm: {
      listProviders: async () => {
        const providers: LlmProviderInfo[] = []
        const groups = usableGroups()
        for (const group of groups) providers.push({ id: group.id, name: group.name })
        return OK(providers)
      },
      listConfigurableProviders: async () => {
        const directory: LlmConfigurableProvider[] = [
          {
            provider: 'deepseek-account', displayName: 'DeepSeek Account',
            settingsNs: 'llm-deepseek-account', settingsPath: [],
            nativeAuth: { env: [], oauth: true },
          },
          {
            provider: 'deepseek', displayName: 'DeepSeek',
            settingsNs: 'llm-deepseek', settingsPath: [],
            defaultBaseURL: 'https://api.deepseek.com',
          },
        ]
        for (const [provider, profile] of declaredProfiles()) {
          directory.push({
            provider,
            displayName: typeof profile.displayName === 'string' && profile.displayName.length > 0 ? profile.displayName : provider,
            settingsNs: 'llm-pi-ai', settingsPath: ['providers', provider], declared: true,
            ...(typeof profile.baseURL === 'string' ? { defaultBaseURL: profile.baseURL } : {}),
          })
        }
        return OK(directory)
      },
      discoverModels: async (settingsNs, request) => {
        if (!sections.has(settingsNs)) {
          return FAIL<LlmDiscoveredModel[]>('llm/model-discovery-rejected', `没有名为 ${settingsNs} 的服务商配置域`)
        }
        if (typeof request.baseURL !== 'string' || !/^https?:\/\//.test(request.baseURL)) {
          return FAIL<LlmDiscoveredModel[]>('llm/model-discovery-rejected', '该路由需要一个 http(s) 端点才能列模型')
        }
        // 本地假目录：不触网，端点只用来决定命名，保证同一端点答案稳定。
        const host = new URL(request.baseURL).hostname.replace(/[^a-z0-9]+/gi, '-').toLowerCase()
        return OK<LlmDiscoveredModel[]>([
          { id: `${host}-pro`, name: `${host} Pro`, contextWindow: 128_000, maxTokens: 8_192, inputModalities: ['text'] },
          { id: `${host}-mini`, name: `${host} Mini`, contextWindow: 64_000, maxTokens: 4_096, inputModalities: ['text'] },
        ])
      },
    },
    credentials: {
      describe: async (refs) => {
        if (refs.length > MAX_DESCRIBE_REFS) {
          return FAIL<Record<string, CredentialInfo>>('credentials/rejected', `一次最多描述 ${MAX_DESCRIBE_REFS} 个引用`)
        }
        const info: Record<string, CredentialInfo> = {}
        for (const ref of refs) {
          const configured = credentials.has(ref)
          info[ref] = { configured, ...(configured ? { source: 'settings' } : {}), writable: true }
        }
        return OK(info)
      },
      set: async (ref, value) => {
        if (!CREDENTIAL_REF.test(ref)) return FAIL<void>('credentials/rejected', `非法密钥引用：${ref}`)
        if (value.length === 0) return FAIL<void>('credentials/rejected', '密钥不能为空')
        credentials.set(ref, value)
        emit('credentials/reference-updated', ref)
        emit('llm/adapters-updated')
        return OK(undefined)
      },
      unset: async (ref) => {
        if (!CREDENTIAL_REF.test(ref)) return FAIL<void>('credentials/rejected', `非法密钥引用：${ref}`)
        credentials.delete(ref)
        emit('credentials/reference-updated', ref)
        emit('llm/adapters-updated')
        return OK(undefined)
      },
    },
    permissionDefault: () => {
      const view = viewOf('permission')
      const value = view === undefined ? undefined : at(view.value, ['defaultPreset'])
      return typeof value === 'string' && value.length > 0 ? value : undefined
    },
    session: {
      modelCatalog: async () => {
        const groups = usableGroups()
        const routableProviders = groups.map(group => group.id)
        const heldGroup = groups.find(group => group.id === selection.provider)
        const heldModel = heldGroup?.models.find(model => model.id === selection.model)
        const chosenGroup = heldModel === undefined ? groups[0] : heldGroup
        const chosenModel = heldModel ?? chosenGroup?.models[0]
        // 席位每次读目录都按"当前选择 + 该模型可用档位"解析有效 effort：
        // 换了模型时旧档作废，避免把一个模型的推理档带到另一个模型上。
        const defaultSelection = (): ModelSelection => {
          if (chosenGroup === undefined || chosenModel === undefined) return selection
          const effort = selection.reasoningEffort
          const usable = effort !== undefined && chosenModel.reasoning?.efforts.some(option => option.id === effort)
          const reasoningEffort = usable ? effort : chosenModel.reasoning?.defaultEffort
          return {
            provider: chosenGroup.id,
            model: chosenModel.id,
            ...(reasoningEffort === undefined ? {} : { reasoningEffort }),
          }
        }
        return OK<ModelCatalog>({
          default: defaultSelection(),
          routableProviders,
          groups,
          failures: [],
        })
      },
      selectModel: async (request) => {
        selection = { ...request }
        options.onSelectionChanged?.(selection)
        return OK({ selected: selection })
      },
    },
  }
}
