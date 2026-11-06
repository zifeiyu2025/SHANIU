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

const envelope = (schema: { toJSON(): unknown }): JsonValue =>
  JSON.parse(JSON.stringify(schema.toJSON())) as JsonValue

const DEEPSEEK_MODELS: JsonValue = [
  { id: 'deepseek-v4-flash', name: 'DeepSeek-V4-Flash', description: '', contextWindow: 1_000_000, maxTokens: 65_536 },
  { id: 'deepseek-v4-pro', name: 'DeepSeek-V4-Pro', description: '', contextWindow: 1_000_000, maxTokens: 131_072 },
]

/** 内建目录模型的统一推理档：官方按模型各异，mock 只做界面展示，用一套通用档位。 */
const CATALOG_REASONING_EFFORTS = { off: null, low: 'low', medium: 'medium', high: 'high', max: 'max' }

/** 一个内建服务商（官方 llm-pi-ai 目录项的精简面）。 */
interface CatalogProvider {
  readonly displayName: string
  readonly defaultBaseURL: string
  /** 适配器原生读取的密钥环境变量（第一个作为首选提示）。 */
  readonly env: readonly string[]
  readonly models: readonly CatalogModel[]
}

interface CatalogModel {
  readonly id: string
  readonly name: string
  readonly contextWindow: number
  readonly maxTokens: number
  readonly reasoning: boolean
}

/**
 * 第三方服务商目录的子集：id / 模型名 / 上下文 / 输出上限逐字取自官方
 * `@earendil-works/pi-ai@1.3.0` 的模型数据，端点和密钥环境变量取自 DSH 的
 * `provider-data.generated.ts`。完整目录（41 家）属于后端适配器的接线面，
 * mock 只收录 SHANIU 常用的七家，避免把整份目录塞进前端包。
 */
const CATALOG: Record<string, CatalogProvider> = {
  openai: {
    displayName: 'OpenAI',
    defaultBaseURL: 'https://api.openai.com/v1',
    env: ['OPENAI_API_KEY'],
    models: [
      { id: 'gpt-6.1-sol', name: 'GPT-6.1 Sol', contextWindow: 272_000, maxTokens: 128_000, reasoning: true },
      { id: 'gpt-6-sol', name: 'GPT-6 Sol', contextWindow: 272_000, maxTokens: 128_000, reasoning: true },
      { id: 'gpt-6-luna', name: 'GPT-6 Luna', contextWindow: 272_000, maxTokens: 128_000, reasoning: true },
      { id: 'o3-pro', name: 'o3-pro', contextWindow: 200_000, maxTokens: 100_000, reasoning: true },
      { id: 'o4-mini', name: 'o4-mini', contextWindow: 200_000, maxTokens: 100_000, reasoning: true },
    ],
  },
  anthropic: {
    displayName: 'Anthropic',
    defaultBaseURL: 'https://api.anthropic.com',
    env: ['ANTHROPIC_API_KEY'],
    models: [
      { id: 'claude-opus-5-5', name: 'Claude Opus 5.5', contextWindow: 1_000_000, maxTokens: 128_000, reasoning: true },
      { id: 'claude-sonnet-5-5', name: 'Claude Sonnet 5.5', contextWindow: 1_000_000, maxTokens: 128_000, reasoning: true },
      { id: 'claude-opus-5', name: 'Claude Opus 5', contextWindow: 1_000_000, maxTokens: 128_000, reasoning: true },
      { id: 'claude-sonnet-5', name: 'Claude Sonnet 5', contextWindow: 1_000_000, maxTokens: 128_000, reasoning: true },
      { id: 'claude-opus-4-8', name: 'Claude Opus 4.8', contextWindow: 1_000_000, maxTokens: 128_000, reasoning: true },
    ],
  },
  google: {
    displayName: 'Google',
    defaultBaseURL: 'https://generativelanguage.googleapis.com/v1beta',
    env: ['GEMINI_API_KEY'],
    models: [
      { id: 'gemini-4-argon', name: 'Gemini 4 Argon', contextWindow: 1_048_576, maxTokens: 32_768, reasoning: true },
      { id: 'gemini-3.8-flash', name: 'Gemini 3.8 Flash', contextWindow: 1_048_576, maxTokens: 65_536, reasoning: true },
      { id: 'gemini-3.1-pro-preview', name: 'Gemini 3.1 Pro Preview', contextWindow: 1_048_576, maxTokens: 65_536, reasoning: true },
      { id: 'gemini-flash-latest', name: 'Gemini Flash Latest', contextWindow: 1_048_576, maxTokens: 65_536, reasoning: true },
    ],
  },
  xai: {
    displayName: 'xAI',
    defaultBaseURL: 'https://api.x.ai/v1',
    env: ['XAI_API_KEY'],
    models: [
      { id: 'grok-4.7', name: 'Grok 4.7', contextWindow: 500_000, maxTokens: 500_000, reasoning: true },
      { id: 'grok-4.6', name: 'Grok 4.6', contextWindow: 500_000, maxTokens: 500_000, reasoning: true },
      { id: 'grok-4.5', name: 'Grok 4.5', contextWindow: 500_000, maxTokens: 500_000, reasoning: true },
      { id: 'grok-4.3', name: 'Grok 4.3', contextWindow: 1_000_000, maxTokens: 30_000, reasoning: true },
    ],
  },
  moonshotai: {
    displayName: 'Moonshot AI',
    defaultBaseURL: 'https://api.moonshot.ai/v1',
    env: ['MOONSHOT_API_KEY'],
    models: [
      { id: 'kimi-k3', name: 'Kimi K3', contextWindow: 1_048_576, maxTokens: 1_048_576, reasoning: true },
      { id: 'kimi-k2.7-code', name: 'Kimi K2.7 Code', contextWindow: 262_144, maxTokens: 262_144, reasoning: true },
      { id: 'kimi-k2.7-code-highspeed', name: 'Kimi K2.7 Code HighSpeed', contextWindow: 262_144, maxTokens: 262_144, reasoning: true },
      { id: 'kimi-k2.6', name: 'Kimi K2.6', contextWindow: 262_144, maxTokens: 262_144, reasoning: true },
    ],
  },
  zai: {
    displayName: 'Z.ai',
    defaultBaseURL: 'https://api.z.ai/api/coding/paas/v4',
    env: ['ZAI_API_KEY'],
    models: [
      { id: 'glm-5.3', name: 'GLM-5.3', contextWindow: 1_000_000, maxTokens: 131_072, reasoning: true },
      { id: 'glm-5.3-flash', name: 'GLM-5.3-Flash', contextWindow: 1_000_000, maxTokens: 131_072, reasoning: true },
      { id: 'glm-5.3-highspeed', name: 'GLM-5.3 Highspeed', contextWindow: 1_000_000, maxTokens: 131_072, reasoning: true },
      { id: 'glm-5.2', name: 'GLM-5.2', contextWindow: 1_000_000, maxTokens: 131_072, reasoning: true },
    ],
  },
  groq: {
    displayName: 'Groq',
    defaultBaseURL: 'https://api.groq.com/openai/v1',
    env: ['GROQ_API_KEY'],
    models: [
      { id: 'openai/gpt-oss-120b', name: 'GPT OSS 120B', contextWindow: 131_072, maxTokens: 65_536, reasoning: true },
      { id: 'llama-3.3-70b-versatile', name: 'Llama 3.3 70B', contextWindow: 131_072, maxTokens: 32_768, reasoning: false },
      { id: 'qwen/qwen3.8-27b', name: 'Qwen3.8 27B', contextWindow: 131_042, maxTokens: 16_384, reasoning: true },
    ],
  },
}

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
    ['llm-pi-ai', {
      schema: envelope(piAiConfig),
      base: { providers: {} },
      // 出厂没有任何自定义服务商：设置-模型只列官方 DeepSeek 行（未配密钥），
      // 自定义路由由用户在「添加模型提供商」里现场写入（与真实写路径同一份文档）。
      user: {},
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

  // 出厂不配任何密钥：席位在未配置时显示「模型配置」，按设置页填好才出现可用模型
  const credentials = new Map<string, string>()
  let selection: ModelSelection = { provider: '', model: '' }

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

  /** 目录项的模型行：带 reasoningEfforts 的就有推理档（与手声明 profile 同一形状）。 */
  const catalogRows = (entry: CatalogProvider): unknown[] =>
    entry.models.map(model => ({
      id: model.id,
      name: model.name,
      ...(model.reasoning ? { reasoningEfforts: CATALOG_REASONING_EFFORTS } : {}),
    }))

  /**
   * 一个可路由的服务商分组；字段优先取 profile，缺省回落到内建目录——
   * 与官方 pi-ai 同口径：路由名命中已安装服务商时，端点/模型/鉴权都由目录补齐。
   */
  const usableGroup = (provider: string, profile: Record<string, JsonValue>): ModelProviderGroup | undefined => {
    const catalog = CATALOG[provider]
    const baseURL = typeof profile.baseURL === 'string' && profile.baseURL.length > 0
      ? profile.baseURL : catalog?.defaultBaseURL
    const declaredModels = Array.isArray(profile.models) ? profile.models : []
    const models = declaredModels.length > 0 ? declaredModels : (catalog === undefined ? [] : catalogRows(catalog))
    const keyRef = typeof profile.apiKeyEnv === 'string' ? profile.apiKeyEnv : catalog?.env[0]
    if (baseURL === undefined || baseURL.length === 0 || models.length === 0 || !keyReady(keyRef)) return undefined
    return {
      id: provider,
      name: typeof profile.displayName === 'string' && profile.displayName.length > 0
        ? profile.displayName
        : catalog?.displayName ?? provider,
      models: catalogModels(models),
    }
  }

  /** 一个可路由的 provider 分组；目录项与席位分组由同一判定产生。 */
  const usableGroups = (): ModelProviderGroup[] => {
    const groups: ModelProviderGroup[] = []
    const deepseek = viewOf('llm-deepseek')
    if (deepseek !== undefined && keyReady(at(deepseek.value, ['apiKeyEnv']) as string | undefined)) {
      const models = at(deepseek.value, ['models'])
      if (Array.isArray(models) && models.length > 0) groups.push({ id: 'deepseek', name: 'DeepSeek', models: catalogModels(models, DEEPSEEK_REASONING) })
    }
    for (const [provider, profile] of declaredProfiles()) {
      const group = usableGroup(provider, profile)
      if (group !== undefined) groups.push(group)
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
        const directory = new Map<string, LlmConfigurableProvider>([
          ['deepseek', {
            provider: 'deepseek', displayName: 'DeepSeek',
            settingsNs: 'llm-deepseek', settingsPath: [],
            defaultBaseURL: 'https://api.deepseek.com',
          }],
        ])
        // 内建目录：适配器自带的服务商（未配置也可从「第三方模型提供商」里添加）
        for (const [provider, entry] of Object.entries(CATALOG)) {
          directory.set(provider, {
            provider,
            displayName: entry.displayName,
            settingsNs: 'llm-pi-ai',
            settingsPath: ['providers', provider],
            declared: false,
            defaultBaseURL: entry.defaultBaseURL,
            ...(entry.env.length === 0 ? {} : { nativeAuth: { env: [...entry.env], oauth: false } }),
          })
        }
        // 用户写入的 profile：命中内建目录时仍算内建（官方同口径），否则是手声明路由
        for (const [provider, profile] of declaredProfiles()) {
          const catalog = CATALOG[provider]
          directory.set(provider, {
            provider,
            displayName: typeof profile.displayName === 'string' && profile.displayName.length > 0
              ? profile.displayName
              : catalog?.displayName ?? provider,
            settingsNs: 'llm-pi-ai',
            settingsPath: ['providers', provider],
            ...(catalog === undefined
              ? { declared: true, ...(typeof profile.baseURL === 'string' ? { defaultBaseURL: profile.baseURL } : {}) }
              : {
                declared: false,
                defaultBaseURL: catalog.defaultBaseURL,
                ...(catalog.env.length === 0 ? {} : { nativeAuth: { env: [...catalog.env], oauth: false } }),
              }),
          })
        }
        return OK([...directory.values()])
      },
      discoverModels: async (settingsNs, request) => {
        if (!sections.has(settingsNs)) {
          return FAIL<LlmDiscoveredModel[]>('llm/model-discovery-rejected', `没有名为 ${settingsNs} 的服务商配置域`)
        }
        // 路由名命中内建目录：适配器直接答自己的模型，不触网也无需端点（官方同口径）
        const catalog = request.provider === undefined ? undefined : CATALOG[request.provider]
        if (catalog !== undefined) {
          return OK<LlmDiscoveredModel[]>(catalog.models.map(model => ({
            id: model.id,
            name: model.name,
            contextWindow: model.contextWindow,
            maxTokens: model.maxTokens,
            inputModalities: ['text'],
          })))
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
