/**
 * 设置页 / 凭据面 / 模型目录三张面的真实实现（SH-005~008 接真后端）。
 *
 * 事实来源只有一份：宿主的 `[[providers]]` 文档（`get_settings` 读、`save_providers` 写）。
 * 这里不新增任何"只有界面知道"的状态——DSH 设置页写什么、宿主文档就存什么，重启后
 * 界面看到的还是同一份东西。反过来，宿主文档里表达不了的字段（自定义请求头、
 * 推理档、非 OpenAI 兼容协议）在这里**如实拒绝并带原因**，不静默丢弃：静默丢弃的
 * 表现是"用户在页面上加了、指针移开就没了"，那比一条报错难归因得多。
 *
 * 分层口径照 DSH 自己：`base` 空、`user` 装全部服务商。设置页按"在 user 且不在 base
 * 才可删"判定 removable（`ui-settings-models/src/client/store.ts:255`），把服务商放
 * `base` 会让用户删不掉自己刚加的那家。
 */
import z from '@deepseek-ai/schemastery'
import { RemoteError } from '@deepseek-ai/dsh-typert-protocol'
import { WELCOME_NOTICE_ACK_FIELD, WELCOME_NOTICE_VERSION } from '@deepseek-ai/dsh-client-ui-settings-models/onboarding-copy'
import type { CredentialInfo } from '@deepseek-ai/dsh-credentials/types'
import type { LlmConfigurableProvider, LlmDiscoveredModel, LlmModelDiscoveryRequest, LlmProviderInfo } from '@deepseek-ai/dsh-llm/types'
import type { ModelCatalog, ModelCatalogFailure, ModelProviderGroup, ModelSelection } from '@deepseek-ai/dsh-api-session-controller/types'
import type { SettingsDescribeValue, SettingsNamespaceView, SettingsPathOpView } from '@deepseek-ai/dsh-settings/types'
import type { JsonValue } from '@deepseek-ai/dsh-util-values'
import { PROVIDER_CATALOG } from './provider-catalog'
import type { HostBridge } from './tauri'

/** `[[providers]]` 一行的模型档位（宿主回显是 camelCase，写入用 snake_case）。 */
interface HostModelRow {
  readonly id: string
  readonly name: string
  readonly contextWindow: number
  readonly maxTokens: number
  readonly input: readonly string[]
  /**
   * 推理档位声明（SH-096）：档位名 → wire 值；**空串 = off 档**（不发字段）——
   * TOML 没有 null，宿主文档用空串承载上游的 null，视图与写面在此双向翻译。
   */
  readonly reasoningEfforts?: Readonly<Record<string, string>>
}

/** `get_settings` 里一家服务商的完整回显。 */
interface HostProviderRow {
  readonly id: string
  readonly label: string
  readonly provider: string
  readonly baseUrl: string
  readonly model: string
  /** 席位选中的推理档位（档位名）；空 = 服务商默认（不发字段）。 */
  readonly reasoningEffort?: string
  /** 自定义请求头（SH-097）：发往该端点的附加头，设置页可编辑。 */
  readonly headers?: Readonly<Record<string, string>>
  /** 生效的密钥文件名：显式名或按 id 派生名，由宿主算好。 */
  readonly secretRef: string
  readonly hasApiKey: boolean
  readonly models: readonly HostModelRow[]
}

/** `get_settings` 的视图（04 §1.3）。字段名跟着 Rust 的 json! 走，不重命名。 */
interface SettingsSnapshot {
  readonly baseUrl: string
  readonly model: string
  readonly maxTurns: number
  readonly workspaceRoot: string
  readonly hasApiKey: boolean
  readonly sandboxMode: string
  readonly approvalMode: string
  readonly activeProvider: string
  readonly providerRevision: number
  readonly providers: readonly HostProviderRow[]
}

/** `save_providers` 写入的一行：04 §4 的落盘名。Tauri 只转顶层参数名，嵌套结构体按 serde 名走。 */
interface ProviderPayload {
  readonly id: string
  readonly label: string
  readonly provider: string
  readonly base_url: string
  readonly model: string
  /** 席位选中的推理档位（档位名）。设置页不编辑，从快照透传防丢失。 */
  readonly reasoning_effort: string
  /** 自定义请求头（SH-097）：设置页可编辑，整份替换写面原样承载。 */
  readonly headers?: Record<string, string>
  readonly secret_ref: string
  readonly models: readonly {
    id: string
    name: string
    context_window: number
    max_tokens: number
    input: string[]
    /** 档位 → wire 值；空串 = off（上游 null 的落盘形态）。 */
    reasoning_efforts?: Record<string, string>
  }[]
}

export interface SettingsFace {
  readonly settings: {
    describe(): Promise<FaceResult<SettingsDescribeValue>>
    mutate(ns: string, ops: readonly SettingsPathOpView[], expectedRevision?: number): Promise<FaceResult<SettingsNamespaceView>>
  }
  readonly llm: {
    listProviders(): Promise<FaceResult<LlmProviderInfo[]>>
    listConfigurableProviders(): Promise<FaceResult<LlmConfigurableProvider[]>>
    discoverModels(settingsNs: string, request: LlmModelDiscoveryRequest): Promise<FaceResult<LlmDiscoveredModel[]>>
  }
  readonly credentials: {
    describe(refs: readonly string[]): Promise<FaceResult<Record<string, CredentialInfo>>>
    set(ref: string, value: string): Promise<FaceResult<void>>
    unset(ref: string): Promise<FaceResult<void>>
  }
  readonly session: {
    modelCatalog(): Promise<FaceResult<ModelCatalog>>
    selectModel(request: ModelSelection): Promise<FaceResult<{ selected: ModelSelection }>>
  }
  /** 席位投影用的生效档；不可路由时两个键都留空（未配置的提示态）。 */
  activeSelection(): ModelSelection
  /** 宿主当前工作区根（`[workspace].root`）；读不到文档时是空串。 */
  workspaceRoot(): string
  /** 安全场当前沙箱档（`get_settings` 的 sandboxMode：read_only / workspace_write / danger_full_access）。 */
  sandboxMode(): string
  /**
   * 生效模型的上下文窗口（`[[providers]]` 的 contextWindow）。未配置或为 0 时 undefined：
   * 没有真实容量就不给分母，占用仪表按"数据缺席"隐藏，不猜一个数。
   */
  activeContextWindow(): number | undefined
  /**
   * 把沙箱档写进宿主 config（`save_settings` 的 sandbox_mode 参数）。其余必填参数
   * （base_url/model/workspace_root）传快照现值原样写回，宿主按"只改沙箱档"的保存处理；
   * approval_mode 不传 = 不动那一维（后端裁决矩阵当前不消费它）。读不到文档或宿主
   * 拒绝时抛错，由调用方映射成 wire 失败。
   */
  applySandboxMode(sandboxMode: string): Promise<void>
  /** 强制回宿主重读一次。装配时与外部改动后调用；失败保留上一次缓存（见 read 的注释）。 */
  refresh(): Promise<void>
}

export interface SettingsFaceOptions {
  /** 向客户端事件面广播（typert 的 `$on` 注册表由适配层持有）。 */
  emit(name: string, ...args: unknown[]): void
}

type FaceResult<T> = { readonly ok: true; readonly value: T } | { readonly ok: false; readonly error: RemoteError }

const ok = <T>(value: T): FaceResult<T> => ({ ok: true, value })
const refused = <T>(error: RemoteError): FaceResult<T> => ({ ok: false, error })

/** 设置页写面里一个服务商 profile 允许的键；其余一律拒绝（见 PROFILE_KEYS 的注释）。 */
const PROFILE_KEYS = new Set(['displayName', 'apiKeyEnv', 'api', 'baseURL', 'models', 'headers'])
/**
 * 一行模型允许的键（SH-096 起 `reasoningEfforts` 接线）。不含 `description`：
 * schema 里没声明的字段界面不会给，真给了就报错而不是收下再丢。
 */
const MODEL_KEYS = new Set(['id', 'name', 'contextWindow', 'maxTokens', 'input', 'reasoningEfforts'])

/** 推理档位全集（SH-096，上游 THINKING_LEVELS；后端校验/展示顺序共用）。 */
const THINKING_LEVELS = ['off', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max'] as const
const THINKING_LEVEL_SET = new Set<string>(THINKING_LEVELS)

/** 宿主协议族 ⇄ DSH 的 `api` 名。两边都只认这两个值，其余按"未实现"处理。 */
const API_OF_HOST: Record<string, string> = { 'openai-chat': 'openai-completions', 'openai-responses': 'openai-responses' }
const HOST_OF_API: Record<string, string> = { 'openai-completions': 'openai-chat', 'openai-responses': 'openai-responses' }

/** DSH 的凭据 ref 语法（POSIX 环境变量名）。宿主侧的 DPAPI 文件名清洗是剔除字符， */
const REF_GRAMMAR = /^[A-Za-z_][A-Za-z0-9_]*$/
/** 与设置页 `CustomProviderCard` 的路由键语法一致，界面写得出的名字这里一定收。 */
const ROUTE_GRAMMAR = /^[a-z][a-z0-9]*(?:-[a-z0-9]+)*$/
const MAX_DESCRIBE_REFS = 64
/** 密钥来源标注：DPAPI 存储，不是 DSH 的 settings 文档也不是环境变量。 */
const CREDENTIAL_SOURCE = 'shaniu-dpapi'

/** 设置页的 profile 形状：只声明 SHANIU 后端真能生效的字段。 */
const modelRowSchema = z.object({
  id: z.string().required(),
  name: z.string(),
  contextWindow: z.number().step(1).min(1),
  maxTokens: z.number().step(1).min(1),
  input: z.array(z.union(['text', 'image'])),
  // 推理档位（SH-096）：值是 string 或 null（off 档）。schema 与上游
  // llm-pi-ai config.ts 同构——设置页的档位编辑器从这里的键 union 取可选档。
  reasoningEfforts: z.union([z.const(false), z.dict(z.union([z.string(), z.const(null)]), z.union(THINKING_LEVELS as unknown as string[]))]),
})

const piAiSchema = z.object({
  providers: z.dict(z.object({
    displayName: z.string(),
    apiKeyEnv: z.string().role('credential-ref'),
    api: z.union(['openai-completions', 'openai-responses']),
    baseURL: z.string(),
    // 自定义请求头（SH-097）：schemastery 按这个 dict 渲染编辑器
    headers: z.dict(z.string()),
    models: z.array(modelRowSchema),
  })),
})

const envelope = (schema: { toJSON(): unknown }): JsonValue => JSON.parse(JSON.stringify(schema.toJSON())) as JsonValue
const PI_SCHEMA = envelope(piAiSchema)
const GENERAL_SCHEMA = envelope(z.object({ welcomeNoticeVersion: z.string() }))

const isRecord = (value: unknown): value is Record<string, JsonValue> =>
  typeof value === 'object' && value !== null && !Array.isArray(value)

/** 拒绝写入时抛它：mutate 统一翻成 `settings/rejected`，界面按原样显示 message。 */
class Refusal extends Error {}

/**
 * 按 op 写一份路径寻址的文档。语义照 DSH：`set` 逐级补中间对象、`unset` 删末级键、
 * 空路径整段替换。设置页的删除服务商就是一个 `unset(['providers', id])`。
 */
function applyOps(root: JsonValue, ops: readonly SettingsPathOpView[]): JsonValue {
  let next: JsonValue = structuredClone(root)
  for (const op of ops) {
    if (op.path.length === 0) {
      next = op.op === 'set' && isRecord(op.value) ? structuredClone(op.value) : {}
      continue
    }
    if (!isRecord(next)) next = {}
    const key = op.path[op.path.length - 1] as string
    let cursor = next
    for (const segment of op.path.slice(0, -1)) {
      const child = cursor[segment]
      if (isRecord(child)) cursor = child
      else {
        const created: Record<string, JsonValue> = {}
        cursor[segment] = created
        cursor = created
      }
    }
    if (op.op === 'set') cursor[key] = structuredClone(op.value)
    else Reflect.deleteProperty(cursor, key)
  }
  return next
}

export function createSettingsFace(bridge: HostBridge, options: SettingsFaceOptions): SettingsFace {
  let snapshot: SettingsSnapshot | undefined
  /** `ui-settings-general` 的用户层只在进程内留存，理由见 viewGeneral 的注释。 */
  let generalUser: JsonValue = {}
  let generalRevision = 0

  const read = async (): Promise<SettingsSnapshot | undefined> => {
    try {
      snapshot = await bridge.invoke<SettingsSnapshot>('get_settings')
      return snapshot
    } catch (reason) {
      // 读不到服务商文档时保持上一次的值：把缓存清空会让界面显示"一家都没配"，
      // 而真正的问题只是这一次 invoke 失败。describe 用 writable:false 拦住写入，
      // 空文档 + 允许写 = 一次提交把用户全部服务商删掉。
      console.warn('[SHANIU] 读取宿主设置失败', reason)
      return undefined
    }
  }

  /** 宿主一行 → 设置页看到的 profile。目录命中的路由把端点与模型补齐（官方 pi-ai 同口径）。 */
  const profileOf = (row: HostProviderRow): Record<string, JsonValue> => {
    const catalog = PROVIDER_CATALOG[row.id]
    const profile: Record<string, JsonValue> = {
      api: API_OF_HOST[row.provider] ?? row.provider,
      baseURL: row.baseUrl !== '' ? row.baseUrl : catalog?.defaultBaseURL ?? '',
      models: modelRowsOf(row, catalog),
      apiKeyEnv: row.secretRef,
    }
    if (row.label !== '') profile.displayName = row.label
    // 自定义请求头（SH-097）随视图透传：设置页整份替换写，视图不带这份就会被
    // 一次提交静默抹掉
    if (row.headers !== undefined && Object.keys(row.headers).length > 0) {
      profile.headers = { ...row.headers }
    }
    return profile
  }

  /**
   * 视图里的模型清单：文档有 `models` 就原样给；没有就把生效档那一行 + 目录行合成出来。
   * 合成而不是留空，是为了让"页面显示的行"与"写回文档的行"是同一批——留空的话用户在
   * 页面上改个端点提交，文档里就只有一行，其它行凭空蒸发。
   */
  const modelRowsOf = (row: HostProviderRow, catalog = PROVIDER_CATALOG[row.id]): JsonValue[] => {
    if (row.models.length > 0) {
      return row.models.map(model => {
        const entry: Record<string, JsonValue> = { id: model.id }
        if (model.name !== '') entry.name = model.name
        if (model.contextWindow > 0) entry.contextWindow = model.contextWindow
        if (model.maxTokens > 0) entry.maxTokens = model.maxTokens
        if (model.input.length > 0) entry.input = [...model.input]
        // 推理档声明（SH-096）：空串翻回 null（off 档的上游形态），键按全集顺序
        // 排——设置页的档位编辑器按 dict 条目顺序渲染，BTreeMap 的字母序会让
        // off 排在 low 后面，视觉上像"档位乱序"。
        const declared = model.reasoningEfforts
        if (declared !== undefined && Object.keys(declared).length > 0) {
          const efforts: Record<string, JsonValue> = {}
          for (const level of THINKING_LEVELS) {
            if (!(level in declared)) continue
            const wire = declared[level]
            efforts[level] = wire === '' ? null : wire
          }
          entry.reasoningEfforts = efforts
        }
        return entry
      })
    }
    const seen = new Set<string>()
    const rows: JsonValue[] = []
    const push = (id: string, name: string, contextWindow: number, maxTokens: number): void => {
      if (id === '' || seen.has(id)) return
      seen.add(id)
      const entry: Record<string, JsonValue> = { id }
      if (name !== '') entry.name = name
      if (contextWindow > 0) entry.contextWindow = contextWindow
      if (maxTokens > 0) entry.maxTokens = maxTokens
      rows.push(entry)
    }
    push(row.model, row.model, 0, 0)
    for (const model of catalog?.models ?? []) push(model.id, model.name, model.contextWindow, model.maxTokens)
    return rows
  }

  const providerDict = (rows: readonly HostProviderRow[]): Record<string, JsonValue> => {
    const out: Record<string, JsonValue> = {}
    for (const row of rows) out[row.id] = profileOf(row)
    return out
  }

  const valueOf = (): JsonValue => ({ providers: providerDict(snapshot?.providers ?? []) })

  const viewOfPi = (): SettingsNamespaceView => ({
    autoGenerate: true,
    ns: 'llm-pi-ai',
    schema: PI_SCHEMA,
    base: { providers: {} },
    user: { providers: providerDict(snapshot?.providers ?? []) },
    value: valueOf(),
    applies: 'live',
    secrets: [],
    revision: snapshot?.providerRevision ?? 0,
  })

  const viewOfGeneral = (): SettingsNamespaceView => ({
    autoGenerate: true,
    ns: 'ui-settings-general',
    schema: GENERAL_SCHEMA,
    // SHANIU 不在 DSH 的预览版计划里：预置成"已确认"，首屏不弹上游的预览说明。
    base: { [WELCOME_NOTICE_ACK_FIELD]: WELCOME_NOTICE_VERSION },
    user: generalUser,
    value: { [WELCOME_NOTICE_ACK_FIELD]: WELCOME_NOTICE_VERSION, ...generalUser as Record<string, JsonValue> },
    applies: 'live',
    secrets: [],
    revision: generalRevision,
  })

  /** 视图里的一行 profile → 文档行。不认识的内容一律抛 Refusal，不静默降级。 */
  const parseProfile = (route: string, profile: JsonValue, previous: HostProviderRow | undefined): ProviderPayload => {
    if (!ROUTE_GRAMMAR.test(route)) throw new Refusal(`服务商路由名 ${route} 不合法（需小写字母开头，可含数字与单个连字符）`)
    if (!isRecord(profile)) throw new Refusal(`服务商 ${route} 的配置必须是对象`)
    for (const key of Object.keys(profile)) {
      if (PROFILE_KEYS.has(key)) continue
      throw new Refusal(`服务商 ${route} 含 SHANIU 不认识的配置项 ${key}`)
    }
    // 自定义请求头（SH-097）：结构由 schema 保证（dict<string>），名字与值的
    // 合法性（头名 token、头注入）交给宿主 validate_providers 统一拦
    const rawHeaders = profile.headers
    let headers: Record<string, string> | undefined
    if (rawHeaders !== undefined) {
      if (!isRecord(rawHeaders)) throw new Refusal(`服务商 ${route} 的 headers 必须是字符串到字符串的映射`)
      headers = {}
      for (const [name, value] of Object.entries(rawHeaders)) headers[name] = String(value)
    } else if (previous?.headers !== undefined && Object.keys(previous.headers).length > 0) {
      // 视图没带（老视图缓存）也保住既有头，等价"没改"
      headers = { ...previous.headers }
    }
    const api = profile.api
    let provider = 'openai-chat'
    if (api !== undefined) {
      if (typeof api !== 'string') throw new Refusal(`服务商 ${route} 的 api 必须是协议名`)
      const mapped = HOST_OF_API[api]
      if (mapped === undefined) {
        throw new Refusal(`SHANIU 后端不支持协议 ${api}，可用：openai-completions / openai-responses`)
      }
      provider = mapped
    }
    const baseURL = profile.baseURL === undefined
      ? previous?.baseUrl ?? ''
      : typeof profile.baseURL === 'string' ? profile.baseURL.trim() : (() => { throw new Refusal(`服务商 ${route} 的 baseURL 必须是字符串`) })()
    const secretRef = profile.apiKeyEnv === undefined
      ? previous?.secretRef ?? ''
      : typeof profile.apiKeyEnv === 'string' ? profile.apiKeyEnv.trim() : (() => { throw new Refusal(`服务商 ${route} 的 apiKeyEnv 必须是字符串`) })()
    if (secretRef !== '' && !REF_GRAMMAR.test(secretRef)) {
      throw new Refusal(`密钥引用名 ${secretRef} 不合法：必须是环境变量写法（字母或下划线开头，只含字母、数字、下划线）`)
    }
    const rawModels = profile.models === undefined
      ? previous === undefined ? [] : modelRowsOf(previous)
      : profile.models
    if (!Array.isArray(rawModels)) throw new Refusal(`服务商 ${route} 的 models 必须是数组`)
    const models = rawModels.map(raw => parseModelRow(route, raw))
    const label = profile.displayName === undefined
      ? previous?.label ?? PROVIDER_CATALOG[route]?.displayName ?? route
      : typeof profile.displayName === 'string' ? profile.displayName.trim() : (() => { throw new Refusal(`服务商 ${route} 的 displayName 必须是字符串`) })()
    return {
      id: route,
      label,
      provider,
      base_url: baseURL,
      model: pickActiveModel(models, previous),
      // 选中档位不在设置页文档里（上游同口径：选中档属于席位状态）——从快照
      // 透传防丢失，只有席位的 selectModel 会写它
      reasoning_effort: previous?.reasoningEffort ?? '',
      ...(headers === undefined ? {} : { headers }),
      secret_ref: secretRef,
      models,
    }
  }

  const parseModelRow = (route: string, raw: unknown): ProviderPayload['models'][number] => {
    if (!isRecord(raw)) throw new Refusal(`服务商 ${route} 有模型行不是对象`)
    for (const key of Object.keys(raw)) {
      if (MODEL_KEYS.has(key)) continue
      throw new Refusal(`服务商 ${route} 的模型行含 SHANIU 不认识的字段 ${key}`)
    }
    const id = typeof raw.id === 'string' ? raw.id.trim() : ''
    if (id === '') throw new Refusal(`服务商 ${route} 有模型行缺 id`)
    const positive = (key: string): number => {
      const value = raw[key]
      if (value === undefined) return 0
      if (typeof value !== 'number' || !Number.isInteger(value) || value <= 0) {
        throw new Refusal(`服务商 ${route} 的模型 ${id} 的 ${key} 必须是正整数`)
      }
      return value
    }
    const input = raw.input === undefined ? [] : raw.input
    if (!Array.isArray(input) || input.some(item => item !== 'text' && item !== 'image')) {
      throw new Refusal(`服务商 ${route} 的模型 ${id} 的 input 只能是 text / image`)
    }
    // 推理档声明（SH-096）：`false` = 显式声明非推理模型，SHANIU 没有可继承的
    // 目录，与缺席等价，归一成空 map；dict 的 null 值（off 档）落盘成空串。
    const rawEfforts = raw.reasoningEfforts
    const reasoning_efforts: Record<string, string> = {}
    if (rawEfforts !== undefined && rawEfforts !== false) {
      if (!isRecord(rawEfforts)) throw new Refusal(`服务商 ${route} 的模型 ${id} 的 reasoningEfforts 必须是档位到 wire 值的映射`)
      for (const [level, wire] of Object.entries(rawEfforts)) {
        if (!THINKING_LEVEL_SET.has(level)) {
          throw new Refusal(`服务商 ${route} 的模型 ${id} 有未知推理档位 ${level}（可用：${THINKING_LEVELS.join('/')}）`)
        }
        const value = wire === null || wire === undefined ? '' : wire
        if (typeof value !== 'string') {
          throw new Refusal(`服务商 ${route} 的模型 ${id} 的推理档位 ${level} 的 wire 值必须是字符串或 null`)
        }
        if (level !== 'off' && value.trim() === '') {
          throw new Refusal(`服务商 ${route} 的模型 ${id} 的推理档位 ${level} 缺 wire 值（只有 off 允许为空）`)
        }
        reasoning_efforts[level] = value
      }
      if (Object.values(reasoning_efforts).every(v => v.trim() === '')) {
        throw new Refusal(`服务商 ${route} 的模型 ${id} 的推理档位只声明了 off：至少给一个非 off 档 wire 值`)
      }
    }
    return {
      id,
      name: typeof raw.name === 'string' ? raw.name : id,
      context_window: positive('contextWindow'),
      max_tokens: positive('maxTokens'),
      input: input as string[],
      ...(Object.keys(reasoning_efforts).length > 0 ? { reasoning_efforts } : {}),
    }
  }

  /** 生效模型：原来那行还在就保持不动，否则取清单第一行。清单空则回到单行姿态。 */
  const pickActiveModel = (models: ProviderPayload['models'], previous: HostProviderRow | undefined): string => {
    if (models.length === 0) return previous?.model ?? ''
    return previous !== undefined && models.some(model => model.id === previous.model) ? previous.model : models[0]!.id
  }

  /** 整份视图文档 → 服务商写入载荷。previous 用来保住视图不携带的状态（协议族、密钥名等）。 */
  const toPayload = (document: JsonValue): ProviderPayload[] => {
    const providers = isRecord(document) ? document.providers : undefined
    if (!isRecord(providers)) return []
    const previous = new Map((snapshot?.providers ?? []).map(row => [row.id, row]))
    return Object.entries(providers).map(([route, profile]) => parseProfile(route, profile, previous.get(route)))
  }

  /** 拿到宿主文档缓存；已有就直接用。所有读路径都先过这里。 */
  const ensure = async (): Promise<SettingsSnapshot | undefined> => snapshot ?? await read()

  const commit = async (document: JsonValue, expectedRevision: number | undefined): Promise<FaceResult<SettingsNamespaceView>> => {
    let payload: ProviderPayload[]
    try {
      payload = toPayload(document)
    } catch (reason) {
      const message = reason instanceof Refusal ? reason.message : `写入被拒绝：${String(reason)}`
      return refused(new RemoteError('settings/rejected', message, { ns: 'llm-pi-ai' }))
    }
    const previousActive = snapshot?.activeProvider ?? ''
    // active 的落点规则：原来指得到就保持；指不到（空文档、或刚把生效那家删掉）
    // 就落到清单里的第一家。留空会让"刚配好的服务商"永远不进席位——运行时继续用
    // [model] 回落段，用户在设置页添加的那家根本不会被选中过。
    const activeProvider = payload.some(row => row.id === previousActive)
      ? previousActive
      : payload[0]?.id ?? ''
    try {
      await bridge.invoke<number>('save_providers', {
        providers: payload,
        activeProvider,
        expectedRevision: expectedRevision ?? snapshot?.providerRevision,
      })
    } catch (reason) {
      const message = reason instanceof Error ? reason.message : String(reason)
      // 宿主的乐观锁报错翻成 DSH 的 conflict 分支，设置页据此提示"重新读取"而不是把
      // 原始串糊在表单里；其余错误按拒绝原样回显。
      return refused(message.includes('已被改动')
        ? new RemoteError('settings/conflict', message, { ns: 'llm-pi-ai', expected: expectedRevision ?? -1, actual: snapshot?.providerRevision ?? -1 })
        : new RemoteError('settings/rejected', message, { ns: 'llm-pi-ai' }))
    }
    await read()
    options.emit('settings/document-updated', 'llm-pi-ai', snapshot?.providerRevision ?? 0)
    options.emit('llm/adapters-updated')
    return ok(viewOfPi())
  }

  /** 一个路由是否已经具备可跑的条件；不齐的部分按 failures 报给席位，不假装可用。 */
  const routeState = (row: HostProviderRow): { readonly group?: ModelProviderGroup; readonly failure?: ModelCatalogFailure } => {
    const entry = PROVIDER_CATALOG[row.id]
    const name = row.label !== '' ? row.label : entry?.displayName ?? row.id
    if (API_OF_HOST[row.provider] === undefined) {
      return { failure: { id: row.id, name, message: `协议 ${row.provider} 未实现，可用：openai-chat / openai-responses` } }
    }
    if (row.baseUrl === '' && entry === undefined) return { failure: { id: row.id, name, message: '缺端点地址' } }
    if (!row.hasApiKey) return { failure: { id: row.id, name, message: `未配置密钥（引用名 ${row.secretRef}）` } }
    const models = modelRowsOf(row, entry).filter(isRecord)
      .map(model => {
        const out: Record<string, unknown> = { id: String(model.id ?? ''), name: String(model.name ?? model.id ?? '') }
        // 声明了推理档位的模型把可选档带给席位（SH-096）：下拉按 efforts 列档、
        // 按 defaultEffort 预选；未声明的模型不出档位（席位的既有判据）。
        const declared = row.models.find(candidate => candidate.id === out.id)?.reasoningEfforts
        if (declared !== undefined && Object.keys(declared).length > 0) {
          const efforts = THINKING_LEVELS.filter(level => level in declared)
            .map(level => ({ id: level, name: level }))
          const selected = row.reasoningEffort ?? ''
          out.reasoning = {
            efforts,
            ...(selected !== '' && selected in declared ? { defaultEffort: selected } : {}),
          }
        }
        return out
      })
      .filter(model => model.id !== '')
    if (models.length === 0) return { failure: { id: row.id, name, message: '模型清单为空' } }
    return { group: { id: row.id, name, models: models as unknown as ModelProviderGroup['models'] } }
  }

  const catalog = async (): Promise<ModelCatalog> => {
    await ensure()
    const groups: ModelProviderGroup[] = []
    const failures: ModelCatalogFailure[] = []
    for (const row of snapshot?.providers ?? []) {
      const state = routeState(row)
      if (state.group !== undefined) groups.push(state.group)
      if (state.failure !== undefined) failures.push(state.failure)
    }
    return { default: activeSelection(), routableProviders: groups.map(group => group.id), groups, failures }
  }

  /**
   * 席位当前档：只有能路由的服务商才算"已配置"。
   *
   * 只配了 `[model]` 回落段的安装返回空档位（席位显示"模型配置"提示），这是刻意的：
   * `[model].provider`（`openai-responses`）带连字符，不是 DSH 合法的凭据引用名，
   * 席位把它显示出来后又存不回同一份密钥文件——用户在界面里填的 Key 会写到
   * `OPENAI_RESPONSES_API_KEY`，而后端读的是 `openai-responses`。宁可让用户显式
   * 建一家服务商，也不要一个"填了存不进去"的档位。
   */
  function activeSelection(): ModelSelection {
    const view = snapshot
    if (view === undefined || view.providers.length === 0) return { provider: '', model: '' }
    const row = view.providers.find(candidate => candidate.id === view.activeProvider)
    if (row === undefined || routeState(row).failure !== undefined) return { provider: '', model: '' }
    // 选中档位随档回显：席位据此预选推理档（ModelSelect 的 effectiveEffort 链）
    const effort = row.reasoningEffort ?? ''
    return { provider: row.id, model: row.model, ...(effort === '' ? {} : { reasoningEffort: effort }) }
  }

  /** 读不到快照时回落宿主默认档（config.rs 的 SandboxMode `#[default]` = ReadOnly）。 */
  function sandboxMode(): string {
    return snapshot?.sandboxMode ?? 'read_only'
  }

  function activeContextWindow(): number | undefined {
    const selection = activeSelection()
    if (selection.provider === '' || selection.model === '') return undefined
    const row = snapshot?.providers.find(candidate => candidate.id === selection.provider)
    const model = row?.models.find(candidate => candidate.id === selection.model)
    return model !== undefined && model.contextWindow > 0 ? model.contextWindow : undefined
  }

  const applySandboxMode = async (mode: string): Promise<void> => {
    await ensure()
    if (snapshot === undefined) throw new Error('读不到宿主设置，无法切换挡位')
    // 非法档位由宿主 save_settings 校验并报错（lib.rs 的 SANDBOX_MODE_NAMES），
    // 这里不预判一份平行清单——两处清单迟早漂移，宿主那份才是事实来源。
    await bridge.invoke('save_settings', {
      base_url: snapshot.baseUrl,
      model: snapshot.model,
      workspace_root: snapshot.workspaceRoot,
      sandbox_mode: mode,
    })
    await read()
  }

  return {
    settings: {
      describe: async () => {
        await ensure()
        return ok<SettingsDescribeValue>({
          // 读不到宿主文档时必须报不可写：设置页的提交是"整份文档一次写入"，
          // 拿着空文档写回去就等于把用户已有的服务商全删了。
          writable: snapshot !== undefined,
          // 宿主没有"打开配置文件"这条命令，如实报没有文档（界面只据此隐掉按钮）。
          hasDocument: false,
          namespaces: [viewOfPi(), viewOfGeneral()],
        })
      },
      mutate: async (ns, ops, expectedRevision) => {
        if (ns === 'ui-settings-general') {
          generalUser = applyOps(structuredClone(generalUser), ops)
          generalRevision += 1
          options.emit('settings/document-updated', ns, generalRevision)
          return ok(viewOfGeneral())
        }
        if (ns !== 'llm-pi-ai') {
          return refused(new RemoteError('settings/rejected', `SHANIU 没有 ${ns} 这个配置域`, { ns }))
        }
        await ensure()
        if (snapshot === undefined) {
          return refused(new RemoteError('settings/rejected', '读不到宿主服务商文档，未写入', { ns }))
        }
        if (expectedRevision !== undefined && expectedRevision !== snapshot.providerRevision) {
          return refused(new RemoteError('settings/conflict', '服务商文档已被改动，请重新读取后再写', {
            ns, expected: expectedRevision, actual: snapshot.providerRevision,
          }))
        }
        return commit(applyOps(valueOf(), ops), expectedRevision)
      },
    },
    llm: {
      listProviders: async () => {
        const view = await catalog()
        return ok<LlmProviderInfo[]>(view.groups.map(group => ({ id: group.id, name: group.name })))
      },
      listConfigurableProviders: async () => {
        await ensure()
        const directory = new Map<string, LlmConfigurableProvider>()
        for (const [route, entry] of Object.entries(PROVIDER_CATALOG)) {
          directory.set(route, {
            provider: route,
            displayName: entry.displayName,
            settingsNs: 'llm-pi-ai',
            settingsPath: ['providers', route],
            declared: false,
            defaultBaseURL: entry.defaultBaseURL,
            // 不报 nativeAuth：SHANIU 后端只从 DPAPI 取密钥，不读宿主环境变量，
            // 报了就会让界面把"环境变量里已经有 Key"当成一种可用状态。
          })
        }
        for (const row of snapshot?.providers ?? []) {
          const catalogEntry = PROVIDER_CATALOG[row.id]
          directory.set(row.id, {
            provider: row.id,
            displayName: row.label !== '' ? row.label : catalogEntry?.displayName ?? row.id,
            settingsNs: 'llm-pi-ai',
            settingsPath: ['providers', row.id],
            ...(catalogEntry === undefined
              ? { declared: true, ...(row.baseUrl === '' ? {} : { defaultBaseURL: row.baseUrl }) }
              : { declared: false, defaultBaseURL: catalogEntry.defaultBaseURL }),
          })
        }
        return ok([...directory.values()])
      },
      discoverModels: async (settingsNs, request) => {
        if (settingsNs !== 'llm-pi-ai') {
          return refused(new RemoteError('llm/model-discovery-rejected', `SHANIU 没有 ${settingsNs} 这个配置域`, { settingsNs }))
        }
        const entry = request.provider === undefined ? undefined : PROVIDER_CATALOG[request.provider]
        if (entry !== undefined) {
          return ok<LlmDiscoveredModel[]>(entry.models.map(model => ({
            id: model.id,
            name: model.name,
            contextWindow: model.contextWindow,
            maxTokens: model.maxTokens,
            inputModalities: ['text'],
          })))
        }
        // 自建端点（SH-098）：宿主带密钥访问 GET {baseURL}/models。探针密钥
        // （表单里刚输入未存的）由宿主优先使用、用完即弃。
        const baseURL = (request.baseURL ?? '').trim()
        if (baseURL === '') {
          return refused(new RemoteError('llm/model-discovery-rejected', '端点地址为空：拉取模型需要 baseURL，或手工填写模型行', { settingsNs }))
        }
        const api = request.api
        if (api !== undefined && api !== 'openai-completions' && api !== 'openai-responses') {
          return refused(new RemoteError('llm/model-discovery-rejected',
            `SHANIU 只支持 OpenAI 兼容端点的模型发现（openai-completions / openai-responses），收到 ${api}`,
            { settingsNs, baseURL }))
        }
        try {
          const models = await bridge.invoke<LlmDiscoveredModel[]>('discover_models', {
            provider: request.provider ?? '',
            baseUrl: baseURL,
            apiKey: request.apiKey,
          })
          return ok(models)
        } catch (reason) {
          const message = reason instanceof Error ? reason.message : String(reason)
          return refused(new RemoteError('llm/model-discovery-rejected', message, { settingsNs, baseURL }))
        }
      },
    },
    credentials: {
      describe: async (refs) => {
        if (refs.length > MAX_DESCRIBE_REFS) {
          return refused(new RemoteError('credential/rejected', `一次最多描述 ${MAX_DESCRIBE_REFS} 个引用`, { ref: refs[0] ?? '' }))
        }
        await ensure()
        const info: Record<string, CredentialInfo> = {}
        for (const ref of refs) {
          const row = snapshot?.providers.find(candidate => candidate.secretRef === ref)
          if (row !== undefined) {
            info[ref] = { configured: row.hasApiKey, ...(row.hasApiKey ? { source: CREDENTIAL_SOURCE } : {}), writable: true }
            continue
          }
          // 语法外的名字读不到任何引用，按未配置回答而不是报错——DSH 自己的凭据面
          // 就是这么规定的（credentials/index.ts 的 isCredentialRefName 注释）。
          if (!REF_GRAMMAR.test(ref)) {
            info[ref] = { configured: false, writable: false }
            continue
          }
          const held = await bridge.invoke<boolean>('has_api_key', { provider: ref }).catch((): false => false)
          info[ref] = { configured: held, ...(held ? { source: CREDENTIAL_SOURCE } : {}), writable: true }
        }
        return ok(info)
      },
      set: async (ref, value) => {
        if (!REF_GRAMMAR.test(ref)) {
          return refused(new RemoteError('credential/rejected', `密钥引用名 ${ref} 不合法：必须是环境变量写法`, { ref }))
        }
        if (value.length === 0) {
          return refused(new RemoteError('credential/rejected', '密钥不能为空', { ref }))
        }
        try {
          await bridge.invoke('store_api_key', { provider: ref, key: value })
        } catch (reason) {
          return refused(new RemoteError('credential/rejected', reason instanceof Error ? reason.message : String(reason), { ref }))
        }
        await read()
        options.emit('credentials/reference-updated', ref)
        options.emit('llm/adapters-updated')
        return ok(undefined)
      },
      unset: async (ref) => {
        if (!REF_GRAMMAR.test(ref)) {
          return refused(new RemoteError('credential/rejected', `密钥引用名 ${ref} 不合法：必须是环境变量写法`, { ref }))
        }
        try {
          await bridge.invoke('delete_api_key', { provider: ref })
        } catch (reason) {
          return refused(new RemoteError('credential/rejected', reason instanceof Error ? reason.message : String(reason), { ref }))
        }
        await read()
        options.emit('credentials/reference-updated', ref)
        options.emit('llm/adapters-updated')
        return ok(undefined)
      },
    },
    session: {
      modelCatalog: async () => ok(await catalog()),
      selectModel: async (request) => {
        await ensure()
        if (snapshot === undefined) {
          return refused(new RemoteError('gateway/internal', '读不到宿主服务商文档，无法切换模型', {}))
        }
        const row = snapshot.providers.find(candidate => candidate.id === request.provider)
        if (row === undefined || request.model === '') {
          return refused(new RemoteError('gateway/bad-request', `服务商 ${request.provider} 不在文档里，不能选它的模型`, {}))
        }
        // 席位只能选它列得出来的行。选了清单里没有的模型必须拒绝，而不是写进去：
        // 写进去的那份文档会被 validate_providers 判成"生效模型不在清单里"，
        // 结果是整份服务商文档写不进去，用户以为切换失败是别的原因。
        const selectable = modelRowsOf(row).filter(isRecord).map(model => String(model.id ?? ''))
        if (selectable.length > 0 && !selectable.includes(request.model)) {
          return refused(new RemoteError('gateway/bad-request', `服务商 ${row.id} 的模型清单里没有 ${request.model}`, {}))
        }
        // 选中推理档位（SH-096）：请求带档位时必须是该模型声明过的档位；**缺席 =
        // 保留现有档位**。上游的模型行点击本就不带 effort（ModelSelect.tsx:596），
        // 而 SHANIU 的持久档就在这份文档里——照"缺席即清空"处理会让配置里选好的
        // 档位被一次普通选模型抹掉（真机缺陷）。回到"不发字段"的席位路径是选 off
        // 档（同样不发字段，与上游 provider default 语义对齐）。
        const effort = (request.reasoningEffort ?? '').trim()
        const effectiveEffort = effort !== '' ? effort : row.reasoningEffort ?? ''
        if (effort !== '') {
          const declared = row.models.find(candidate => candidate.id === request.model)?.reasoningEfforts
          if (declared === undefined || !(effort in declared)) {
            return refused(new RemoteError('gateway/bad-request', `模型 ${request.model} 未声明推理档位 ${effort}`, {}))
          }
        }
        const providers = snapshot.providers.map(candidate => ({
          id: candidate.id,
          label: candidate.label,
          provider: candidate.provider,
          base_url: candidate.baseUrl,
          secret_ref: candidate.secretRef,
          // 每行的选中档位原样透传；被选中的那家写生效值。模型行的档位声明同理，
          // 整份替换的写面上丢一个字段 = 用户配置凭空消失。
          reasoning_effort: candidate.id === row.id ? effectiveEffort : candidate.reasoningEffort ?? '',
          // 自定义头（SH-097）整表透传
          ...(candidate.headers === undefined || Object.keys(candidate.headers).length === 0
            ? {}
            : { headers: { ...candidate.headers } }),
          models: candidate.models.map(model => ({
            id: model.id,
            name: model.name,
            context_window: model.contextWindow,
            max_tokens: model.maxTokens,
            input: [...model.input],
            ...(model.reasoningEfforts === undefined || Object.keys(model.reasoningEfforts).length === 0
              ? {}
              : { reasoning_efforts: { ...model.reasoningEfforts } }),
          })),
          model: candidate.id === row.id ? request.model : candidate.model,
        }))
        try {
          await bridge.invoke<number>('save_providers', {
            providers,
            activeProvider: row.id,
            expectedRevision: snapshot.providerRevision,
          })
        } catch (reason) {
          return refused(new RemoteError('gateway/internal', reason instanceof Error ? reason.message : String(reason), {}))
        }
        await read()
        const selected: ModelSelection = {
          provider: row.id,
          model: request.model,
          // 生效档位随应答回显（缺席=保留时的现值），席位据此显示而不回落错位
          ...(effectiveEffort === '' ? {} : { reasoningEffort: effectiveEffort }),
        }
        return ok({ selected })
      },
    },
    activeSelection,
    sandboxMode,
    activeContextWindow,
    applySandboxMode,
    workspaceRoot: () => snapshot?.workspaceRoot ?? '',
    refresh: async () => {
      await read()
    },
  }
}
