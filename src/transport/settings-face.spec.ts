/**
 * 设置页 / 凭据 / 模型目录三张面的契约测试（SH-005~008 接真后端）。
 *
 * 判据不是"返回值长什么样"，而是三类真会咬人的行为：
 * ① 写路径送出的文档形状必须与 04 §4 的落盘名一致（嵌套结构体 serde 不转驼峰）；
 * ② 界面写得出、后端吃不下的东西必须**明确拒绝**，不能静默丢——静默丢的用户-visible
 *    表现是"刚加的模型重载就没了"；
 * ③ 读不到宿主文档时不能允许整份写入，否则一次提交等于删光用户的服务商。
 */
import { describe, expect, it } from 'vitest'
import { createSettingsFace } from './settings-face'
import type { HostBridge } from './tauri'

/** 一个能真的改状态、真的按修订号拒写的宿主替身。 */
interface FakeHost {
  readonly bridge: HostBridge
  readonly calls: { command: string; args: Record<string, unknown> | undefined }[]
  readonly emitted: { name: string; args: unknown[] }[]
  revision: number
  active: string
  failRead: boolean
  /** 宿主 `[sandbox] mode` 的替身状态：`save_settings` 会改它，`get_settings` 按它回显。 */
  sandboxMode: string
  /** discover_models 桩：设了错误就抛，否则回固定清单。 */
  discoverError: string | undefined
  rows(): Row[]
}

interface Row {
  id: string
  label: string
  provider: string
  base_url: string
  model: string
  /** 席位选中的推理档位（档位名）；空 = 服务商默认。 */
  reasoning_effort?: string
  /** 自定义请求头（SH-097）。 */
  headers?: Record<string, string>
  secret_ref: string
  models: { id: string; name: string; context_window: number; max_tokens: number; input: string[]; reasoning_efforts?: Record<string, string> }[]
}

/**
 * Tauri 2 的**参数名映射契约**（SH-105 挡位全失效的真根因）：宿主那侧是裸
 * `#[tauri::command] fn save_settings(base_url: String, …)`（`lib.rs`，没有 `rename_all`），
 * Tauri 默认把 Rust 形参名映射成 JS 侧的 **camelCase**。传 snake_case = 出现未知键 +
 * 缺必填键，宿主直接拒单：
 * `invalid args \`baseUrl\` for command \`save_settings\`: command save_settings missing required key baseUrl`。
 *
 * 这条规则**只作用于顶层参数名**：`save_providers` 的 `providers` 数组元素在 serde 结构体里，
 * 仍按 serde 定义走 snake_case（04 §1.3）——两处不能"统一"。
 *
 * 替身按同形报错，因为"只断言调用发生了"的假桥会让这类集成层缺陷一路绿到真机
 * （挡位层就是这样 192 例全绿、`config.toml` 却一次都没写进去）。
 */
const CAMEL_COMMAND_ARGS: Record<string, readonly string[]> = {
  save_settings: ['baseUrl', 'model', 'workspaceRoot'],
  save_providers: ['providers', 'activeProvider', 'expectedRevision'],
}

function assertCommandArgs(command: string, args: Record<string, unknown> | undefined): void {
  const required = CAMEL_COMMAND_ARGS[command]
  if (required === undefined) return
  const given = args ?? {}
  for (const key of Object.keys(given)) {
    if (key.includes('_')) {
      throw new Error(`invalid args \`${key}\` for command \`${command}\`: unknown key (Tauri maps parameters to camelCase)`)
    }
  }
  for (const key of required) {
    if (!(key in given)) {
      throw new Error(`invalid args \`${key}\` for command \`${command}\`: command ${command} missing required key ${key}`)
    }
  }
}

function fakeHost(rows: Row[] = []): FakeHost {
  const calls: FakeHost['calls'] = []
  const emitted: FakeHost['emitted'] = []
  const keys = new Set<string>()
  const state = {
    revision: 0,
    active: '',
    failRead: false,
    sandboxMode: 'workspace_write',
    rows,
    discoverError: undefined as string | undefined,
  }
  const host: FakeHost = {
    calls,
    emitted,
    get revision() { return state.revision },
    set revision(value) { state.revision = value },
    get active() { return state.active },
    set active(value) { state.active = value },
    get failRead() { return state.failRead },
    set failRead(value) { state.failRead = value },
    get sandboxMode() { return state.sandboxMode },
    set sandboxMode(value) { state.sandboxMode = value },
    get discoverError() { return state.discoverError },
    set discoverError(value) { state.discoverError = value },
    rows: () => state.rows,
    bridge: {
      async invoke<T>(command: string, args?: Record<string, unknown>): Promise<T> {
        calls.push({ command, args })
        // 先按 Tauri 的参数名规则校验：snake_case 顶层键或缺必填键一律拒单
        assertCommandArgs(command, args)
        if (command === 'get_settings') {
          if (state.failRead) throw new Error('宿主忙')
          const active = state.rows[0]
          return {
            baseUrl: active?.base_url ?? '',
            model: active?.model ?? '',
            maxTurns: 32,
            workspaceRoot: 'W',
            hasApiKey: active !== undefined && keys.has(active.secret_ref),
            sandboxMode: state.sandboxMode,
            approvalMode: 'ask',
            activeProvider: state.active,
            providerRevision: state.revision,
            providers: state.rows.map(row => ({
              id: row.id,
              label: row.label,
              provider: row.provider,
              baseUrl: row.base_url,
              model: row.model,
              secretRef: row.secret_ref,
              hasApiKey: keys.has(row.secret_ref),
              // 自定义头（SH-097）按宿主真实形状回显（camelCase 无翻译）
              ...(row.headers === undefined || Object.keys(row.headers).length === 0
                ? {}
                : { headers: row.headers }),
              // 宿主回显是 camelCase，落盘名是 snake_case（04 §4）——替身必须按真实
              // 形状回显，否则这个差异就会在测试里被抹平，而线上不会。
              models: row.models.map(model => ({
                id: model.id,
                name: model.name,
                contextWindow: model.context_window,
                maxTokens: model.max_tokens,
                input: model.input,
                // 空串 = off 档的落盘形态（TOML 无 null），与真实宿主同形
                ...(model.reasoning_efforts === undefined ? {} : { reasoningEfforts: model.reasoning_efforts }),
              })),
              reasoningEffort: row.reasoning_effort ?? '',
            })),
          } as T
        }
        if (command === 'save_providers') {
          const expected = args?.expectedRevision as number | undefined
          if (expected !== undefined && expected !== state.revision) {
            throw new Error(`服务商文档已被改动（期望修订 ${expected}，实际 ${state.revision}），请重新读取后再写`)
          }
          state.rows = args?.providers as Row[]
          state.active = args?.activeProvider as string
          state.revision += 1
          return state.revision as T
        }
        if (command === 'save_settings') {
          // 宿主 `save_settings` 的替身：只改沙箱档（其余必填参数由 face 从快照原样写回）
          state.sandboxMode = String(args?.sandboxMode ?? state.sandboxMode)
          return undefined as T
        }
        if (command === 'has_api_key') return keys.has(args?.provider as string) as T
        if (command === 'store_api_key') {
          keys.add(args?.provider as string)
          return undefined as T
        }
        if (command === 'delete_api_key') {
          keys.delete(args?.provider as string)
          return undefined as T
        }
        if (command === 'discover_models') {
          if (state.discoverError !== undefined) throw new Error(state.discoverError)
          return [
            { id: 'm-a', name: 'Model A', contextWindow: 128_000, maxTokens: 8_192, inputModalities: ['text'] },
            { id: 'm-b', name: null, contextWindow: null, maxTokens: null, inputModalities: ['text'] },
          ] as T
        }
        return undefined as T
      },
      listenEvents(): () => void {
        return () => {}
      },
    },
  }
  return host
}

/**
 * 三张面在测试里的窄形状。真实返回是判别联合，strict 下不能按 `['value']` 取键；
 * 测试判据本来就逐条写死，这里按 Record 取值不影响判据强度。
 */
interface LooseFace {
  readonly settings: {
    describe(): Promise<Record<string, unknown>>
    mutate(ns: string, ops: Record<string, unknown>[], expectedRevision?: number): Promise<Record<string, unknown>>
  }
  readonly llm: {
    listProviders(): Promise<Record<string, unknown>>
    listConfigurableProviders(): Promise<Record<string, unknown>>
    discoverModels(settingsNs: string, request: Record<string, unknown>): Promise<Record<string, unknown>>
  }
  readonly credentials: {
    describe(refs: string[]): Promise<Record<string, unknown>>
    set(ref: string, value: string): Promise<Record<string, unknown>>
    unset(ref: string): Promise<Record<string, unknown>>
  }
  readonly session: {
    modelCatalog(): Promise<Record<string, unknown>>
    selectModel(request: Record<string, unknown>): Promise<Record<string, unknown>>
  }
  activeSelection(): Record<string, unknown>
}

const faceOf = (host: FakeHost): LooseFace => createSettingsFace(host.bridge, {
  emit: (name: string, ...args: unknown[]) => { host.emitted.push({ name, args }) },
}) as unknown as LooseFace

const LONGCAT: Row = {
  id: 'longcat',
  label: 'LongCat',
  provider: 'openai-chat',
  base_url: 'https://api.longcat.chat/openai/v1',
  model: 'LongCat-2.5',
  secret_ref: 'LONGCAT_API_KEY',
  models: [{ id: 'LongCat-2.5', name: 'LongCat 2.5', context_window: 128_000, max_tokens: 8_192, input: ['text'] }],
}

/** 声明了推理档位的变体（SH-096）：off 空串 + 两个非 off 档，选中档 high。 */
const LONGCAT_WITH_EFFORTS: Row = {
  ...LONGCAT,
  reasoning_effort: 'high',
  models: [{
    id: 'LongCat-2.5', name: 'LongCat 2.5', context_window: 128_000, max_tokens: 8_192, input: ['text'],
    reasoning_efforts: { off: '', low: 'low', high: 'ultra' },
  }],
}

/** 声明了推理档位但从未选择过（初始态）：reasoning_effort 为空。 */
const LONGCAT_EFFORTS_UNSELECTED: Row = {
  ...LONGCAT_WITH_EFFORTS,
  reasoning_effort: '',
}

/** 声明了推理档位但没有 high 档（初始默认无从谈起）。 */
const LONGCAT_EFFORTS_NO_HIGH: Row = {
  ...LONGCAT,
  reasoning_effort: '',
  models: [{
    id: 'LongCat-2.5', name: 'LongCat 2.5', context_window: 128_000, max_tokens: 8_192, input: ['text'],
    reasoning_efforts: { off: '', low: 'low' },
  }],
}

function profileValue(overrides: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    displayName: 'MiniMax',
    api: 'openai-completions',
    baseURL: 'https://api.minimax.io/v1',
    apiKeyEnv: 'MINIMAX_API_KEY',
    models: [{ id: 'MiniMax-M2', name: 'MiniMax M2', contextWindow: 200_000, maxTokens: 8_192, input: ['text'] }],
    ...overrides,
  }
}

describe('默认工作区标题/删除标记（上游 registry 可改可删语义的 face 侧）', () => {
  /** 只回一份固定快照的桥：workspace 基线字段的透传测试不关心服务商文档。 */
  const snapshotFace = (snapshot: Record<string, unknown>) => {
    const bridge: HostBridge = {
      async invoke<T>(command: string): Promise<T> {
        if (command === 'get_settings') return snapshot as T
        throw new Error(`unexpected command: ${command}`)
      },
      listenEvents: () => () => {},
    }
    return createSettingsFace(bridge, { emit: () => {} })
  }

  it('宿主回显 workspaceTitle/workspaceHidden 时原样透传', async () => {
    const face = snapshotFace({ workspaceRoot: 'W', workspaceTitle: '主力', workspaceHidden: true })
    await face.refresh()
    expect(face.workspaceTitle()).toBe('主力')
    expect(face.workspaceHidden()).toBe(true)
  })

  it('旧文档缺字段：标题回落上游首启拼写、删除标记回落未删除', async () => {
    const face = snapshotFace({ workspaceRoot: 'W' })
    await face.refresh()
    expect(face.workspaceTitle()).toBe('default-workspace')
    expect(face.workspaceHidden()).toBe(false)
  })
})

describe('settings 面：读视图', () => {
  it('服务商放在 user 层而不是 base 层', async () => {
    const host = fakeHost([LONGCAT])
    host.revision = 7
    host.active = 'longcat'
    const view = (await faceOf(host).settings.describe())
    expect(view['ok']).toBe(true)
    const described = view['value'] as { writable: boolean; hasDocument: boolean; namespaces: Record<string, unknown>[] }
    const piAi = described.namespaces.find(ns => ns['ns'] === 'llm-pi-ai') as {
      base: { providers: Record<string, unknown> }
      user: { providers: Record<string, unknown> }
      value: { providers: Record<string, unknown> }
      revision: number
    }
    // 设置页按"在 user 且不在 base"判定可删（store.ts:255）：放 base 层就删不掉自己加的那家
    expect(Object.keys(piAi.base.providers)).toEqual([])
    expect(Object.keys(piAi.user.providers)).toEqual(['longcat'])
    expect(piAi.value.providers['longcat']).toMatchObject({
      api: 'openai-completions',
      baseURL: 'https://api.longcat.chat/openai/v1',
      displayName: 'LongCat',
      apiKeyEnv: 'LONGCAT_API_KEY',
      models: [{ id: 'LongCat-2.5', name: 'LongCat 2.5', contextWindow: 128_000, maxTokens: 8_192, input: ['text'] }],
    })
    expect(piAi.revision).toBe(7)
    expect(described.hasDocument).toBe(false)
  })

  it('目录命中的路由把端点与模型补齐，视图与写回保持同一批行', async () => {
    const host = fakeHost([{ ...LONGCAT, id: 'openai', label: '', secret_ref: 'OPENAI_API_KEY', base_url: '', model: 'gpt-6-sol', models: [] }])
    const describe = await faceOf(host).settings.describe()
    const piAi = (describe['value'] as { namespaces: Record<string, any>[] })['namespaces']
      .find(ns => ns['ns'] === 'llm-pi-ai') as { value: { providers: Record<string, any> } }
    const profile = piAi.value.providers['openai']
    expect(profile.baseURL).toBe('https://api.openai.com/v1')
    expect((profile.models as unknown[]).map(row => (row as { id: string }).id)).toContain('gpt-6-sol')
    expect((profile.models as unknown[]).length).toBeGreaterThan(1)
  })

  it('读不到宿主文档时报不可写，且拒绝整份写入', async () => {
    const host = fakeHost([LONGCAT])
    host.failRead = true
    const face = faceOf(host)
    const described = await face.settings.describe()
    expect((described['value'] as { writable: boolean }).writable).toBe(false)
    const written = await face.settings.mutate('llm-pi-ai', [{ op: 'set', path: ['providers', 'x'], value: profileValue() }])
    expect(written['ok']).toBe(false)
    expect((written['error'] as Error).message).toContain('读不到')
    // 拿空文档写回去就等于删光用户已有服务商，这条路必须一条命令都不发
    expect(host.calls.some(call => call.command === 'save_providers')).toBe(false)
  })
})

describe('settings 面：写路径', () => {
  it('添加服务商：嵌套载荷用落盘名，提交后广播文档与适配器已变', async () => {
    const host = fakeHost([LONGCAT])
    host.revision = 2
    const face = faceOf(host)
    const result = await face.settings.mutate(
      'llm-pi-ai',
      [{ op: 'set', path: ['providers', 'minimax'], value: profileValue() }],
      2,
    )
    expect(result['ok']).toBe(true)
    const write = host.calls.find(call => call.command === 'save_providers')
    expect(write?.args).toMatchObject({
      // 之前没有生效档（空 active），新文档落回第一家：留着空 active 会让刚配好的
      // 服务商永远不进席位，运行时继续吃 [model] 回落段
      activeProvider: 'longcat',
      expectedRevision: 2,
      providers: [
        { id: 'longcat', secret_ref: 'LONGCAT_API_KEY', base_url: 'https://api.longcat.chat/openai/v1' },
        {
          id: 'minimax',
          label: 'MiniMax',
          provider: 'openai-chat',
          base_url: 'https://api.minimax.io/v1',
          model: 'MiniMax-M2',
          secret_ref: 'MINIMAX_API_KEY',
          models: [{ id: 'MiniMax-M2', name: 'MiniMax M2', context_window: 200_000, max_tokens: 8_192, input: ['text'] }],
        },
      ],
    })
    expect(host.emitted.map(event => event.name)).toEqual(['settings/document-updated', 'llm/adapters-updated'])
    expect((result['value'] as { revision: number }).revision).toBe(3)
  })

  it('只改端点时不动模型清单：视图里没带的字段不能被写成空', async () => {
    const host = fakeHost([LONGCAT])
    host.active = 'longcat'
    const face = faceOf(host)
    await face.settings.mutate('llm-pi-ai', [
      { op: 'set', path: ['providers', 'longcat', 'baseURL'], value: 'https://new/v1' },
    ], host.revision)
    const saved = host.rows()[0]!
    expect(saved.base_url).toBe('https://new/v1')
    expect(saved.models.map(row => row.id)).toEqual(['LongCat-2.5'])
    expect(saved.model).toBe('LongCat-2.5')
    // 密钥名不在本次 op 里，也必须原样保住
    expect(saved.secret_ref).toBe('LONGCAT_API_KEY')
  })

  it('删除当前生效那家时把 active 清空，而不是留一个指不到任何 id 的名字', async () => {
    const host = fakeHost([LONGCAT])
    host.active = 'longcat'
    const face = faceOf(host)
    await face.settings.mutate('llm-pi-ai', [{ op: 'unset', path: ['providers', 'longcat'] }])
    expect(host.rows()).toEqual([])
    expect(host.active).toBe('')
  })

  it('删掉生效那家后落到剩下的第一家，不回落到 [model] 的旧端点', async () => {
    const other: Row = { ...LONGCAT, id: 'minimax', label: 'MiniMax', secret_ref: 'MINIMAX_API_KEY', model: 'MiniMax-M2' }
    const host = fakeHost([LONGCAT, other])
    host.active = 'longcat'
    await faceOf(host).settings.mutate('llm-pi-ai', [{ op: 'unset', path: ['providers', 'longcat'] }])
    // 留空 active 会让运行时静默切回 [model] 回落段——用户删的是一家服务商，不是想换端点
    expect(host.active).toBe('minimax')
  })

  it('修订号对不上时报 conflict 并停在写宿主之前', async () => {
    const host = fakeHost([LONGCAT])
    const result = await faceOf(host).settings.mutate(
      'llm-pi-ai',
      [{ op: 'set', path: ['providers', 'minimax'], value: profileValue() }],
      99,
    )
    expect(result['ok']).toBe(false)
    expect((result['error'] as { code: string }).code).toBe('settings/conflict')
    expect((result['error'] as { details: unknown }).details).toMatchObject({ ns: 'llm-pi-ai', expected: 99, actual: 0 })
    expect(host.calls.some(call => call.command === 'save_providers')).toBe(false)
  })

  it('后端吃不下的写面逐项拒绝并说明原因', async () => {
    const face = faceOf(fakeHost([LONGCAT]))
    // 自定义请求头自 SH-097 起接线，从拒绝清单移除
    const cases: [string, Record<string, unknown>, string][] = [
      ['非 OpenAI 协议', profileValue({ api: 'anthropic-messages' }), '不支持协议'],
      ['非法路由名', profileValue(), ''],
    ]
    for (const [label, value, keyword] of cases) {
      if (keyword === '') continue
      const result = await face.settings.mutate('llm-pi-ai', [{ op: 'set', path: ['providers', 'x'], value }], 0)
      expect(result['ok'], label).toBe(false)
      expect((result['error'] as Error).message, label).toContain(keyword)
    }
    const badRoute = await face.settings.mutate('llm-pi-ai', [{ op: 'set', path: ['providers', 'Bad Route'], value: profileValue() }], 0)
    expect(badRoute['ok']).toBe(false)
    expect((badRoute['error'] as Error).message).toContain('路由名')

    // 推理档自 SH-096 起接线：拒绝断言换成校验断言（未知档位 / 非 off 缺 wire 值）
    const unknownLevel = await face.settings.mutate('llm-pi-ai', [{
      op: 'set',
      path: ['providers', 'x'],
      value: profileValue({ models: [{ id: 'a', reasoningEfforts: { ultra: 'ultra' } }] }),
    }], 0)
    expect(unknownLevel['ok']).toBe(false)
    expect((unknownLevel['error'] as Error).message).toContain('未知推理档位')

    const blankWire = await face.settings.mutate('llm-pi-ai', [{
      op: 'set',
      path: ['providers', 'x'],
      value: profileValue({ models: [{ id: 'a', reasoningEfforts: { low: '' } }] }),
    }], 0)
    expect(blankWire['ok']).toBe(false)
    expect((blankWire['error'] as Error).message).toContain('缺 wire 值')

    const badRef = await face.settings.mutate('llm-pi-ai', [{
      op: 'set', path: ['providers', 'x'], value: profileValue({ apiKeyEnv: '../evil' }),
    }], 0)
    expect(badRef['ok']).toBe(false)
    expect((badRef['error'] as Error).message).toContain('密钥引用名')
  })

  it('推理档声明（SH-096）：null 翻成落盘空串、false 归一缺席，写面带校验', async () => {
    const host = fakeHost([LONGCAT])
    const face = faceOf(host)
    const result = await face.settings.mutate('llm-pi-ai', [{
      op: 'set',
      path: ['providers', 'longcat'],
      value: profileValue({ models: [{ id: 'LongCat-2.5', reasoningEfforts: { off: null, low: 'low', high: 'ultra' } }] }),
    }], 0)
    expect(result['ok']).toBe(true)
    const saved = host.calls.find(call => call.command === 'save_providers')?.args as {
      providers: { models: { reasoning_efforts?: Record<string, string> }[] }[]
    }
    // off 的 null → 空串（TOML 无 null）；wire 值是界面写的自定义拼写
    expect(saved.providers[0]!.models[0]!.reasoning_efforts).toEqual({ off: '', low: 'low', high: 'ultra' })

    // false（显式声明非推理模型）与缺席等价：不产出 reasoning_efforts。
    // 修订号不传 = 用快照现值（上一笔写入已把 revision 推进一格）。
    const disabled = await face.settings.mutate('llm-pi-ai', [{
      op: 'set',
      path: ['providers', 'longcat'],
      value: profileValue({ models: [{ id: 'LongCat-2.5', reasoningEfforts: false }] }),
    }])
    expect(disabled['ok']).toBe(true)
    const saved2 = host.calls.filter(call => call.command === 'save_providers').at(-1)?.args as {
      providers: { models: { reasoning_efforts?: Record<string, string> }[] }[]
    }
    expect(saved2.providers[0]!.models[0]!.reasoning_efforts).toBeUndefined()
  })

  it('视图把落盘空串翻回 null（off 档），键按档位全集顺序排列', async () => {
    const host = fakeHost([LONGCAT_WITH_EFFORTS])
    host.active = 'longcat'
    const view = (await faceOf(host).settings.describe())['value'] as {
      namespaces: { ns: string; user: { providers: Record<string, { models: Record<string, unknown>[] }> } }[]
    }
    const model = view.namespaces.find(ns => ns['ns'] === 'llm-pi-ai')!.user.providers['longcat']!.models[0]!
    expect(model['reasoningEfforts']).toEqual({ off: null, low: 'low', high: 'ultra' })
  })

  it('自定义请求头（SH-097）：写面承载、视图透传、席位切换不丢失', async () => {
    const host = fakeHost([LONGCAT])
    const face = faceOf(host)
    // 写面：headers 随 profile 进 payload
    const written = await face.settings.mutate('llm-pi-ai', [{
      op: 'set', path: ['providers', 'longcat'],
      value: profileValue({ headers: { 'X-Gateway-Sign': 'sig', 'X-Trace': 't1' } }),
    }], 0)
    expect(written['ok']).toBe(true)
    const saved = host.calls.find(call => call.command === 'save_providers')?.args as {
      providers: { headers?: Record<string, string> }[]
    }
    expect(saved.providers[0]!.headers).toEqual({ 'X-Gateway-Sign': 'sig', 'X-Trace': 't1' })

    // 视图透传：快照里的头原样回到设置页
    const view = (await face.settings.describe())['value'] as {
      namespaces: { ns: string; user: { providers: Record<string, Record<string, unknown>> } }[]
    }
    const profile = view.namespaces.find(ns => ns['ns'] === 'llm-pi-ai')!.user.providers['longcat']!
    expect(profile['headers']).toEqual({ 'X-Gateway-Sign': 'sig', 'X-Trace': 't1' })

    // 席位切换整份替换：其它字段透传时头不丢
    await face.session.selectModel({ provider: 'longcat', model: 'LongCat-2.5' })
    const resaved = host.calls.filter(call => call.command === 'save_providers').at(-1)?.args as {
      providers: { headers?: Record<string, string> }[]
    }
    expect(resaved.providers[0]!.headers).toEqual({ 'X-Gateway-Sign': 'sig', 'X-Trace': 't1' })
  })

  it('席位档位（SH-096）：catalog 挂 reasoning，selectModel 持久化档位并校验声明', async () => {
    const host = fakeHost([LONGCAT_WITH_EFFORTS])
    host.active = 'longcat'
    const face = faceOf(host)
    // 可路由（有密钥）才进目录 groups——与既有判据同源
    await face.credentials.set('LONGCAT_API_KEY', 'k')
    // catalog：声明了档位的模型带 reasoning.efforts + defaultEffort；未声明的模型不带
    const catalog = (await face.session.modelCatalog())['value'] as {
      groups: { models: { id: string; reasoning?: { efforts: { id: string }[]; defaultEffort?: string } }[] }[]
    }
    const model = catalog.groups[0]!.models[0]!
    expect(model.reasoning?.efforts.map(effort => effort.id)).toEqual(['off', 'low', 'high'])
    expect(model.reasoning?.defaultEffort).toBe('high')
    expect(face.activeSelection()).toMatchObject({ provider: 'longcat', model: 'LongCat-2.5', reasoningEffort: 'high' })

    // 换档：写入服务商文档的 reasoning_effort（档位名）
    const picked = await face.session.selectModel({ provider: 'longcat', model: 'LongCat-2.5', reasoningEffort: 'low' })
    expect(picked['ok']).toBe(true)
    expect((picked['value'] as { selected: { reasoningEffort?: string } }).selected.reasoningEffort).toBe('low')
    const saved = host.calls.find(call => call.command === 'save_providers')?.args as { providers: { reasoning_effort: string }[] }
    expect(saved.providers[0]!.reasoning_effort).toBe('low')

    // 未声明的档位拒绝：静默收下会让运行时查不到 wire 值、跑的还是默认档
    const bogus = await face.session.selectModel({ provider: 'longcat', model: 'LongCat-2.5', reasoningEffort: 'xhigh' })
    expect(bogus['ok']).toBe(false)
    expect((bogus['error'] as Error).message).toContain('未声明推理档位')

    // 模型行点击不带 effort（上游 ModelSelect.tsx:596 的事实形状）= 保留现有档位，
    // 绝不清空——清空会让"配置选好的 high"被一次普通选模型抹掉（真机缺陷 05:08）
    const keep = await face.session.selectModel({ provider: 'longcat', model: 'LongCat-2.5' })
    expect(keep['ok']).toBe(true)
    expect((keep['value'] as { selected: { reasoningEffort?: string } }).selected.reasoningEffort).toBe('low')
    const resaved = host.calls.filter(call => call.command === 'save_providers').at(-1)?.args as { providers: { reasoning_effort: string }[] }
    expect(resaved.providers[0]!.reasoning_effort).toBe('low')
  })

  it('初始档位默认（用户拍板 2026-11-07）：未选择过时席位预选声明的 high，与请求侧回落同规则', async () => {
    // 未选择（reasoning_effort 空）+ 声明了 high → defaultEffort 预选 high
    const host = fakeHost([LONGCAT_EFFORTS_UNSELECTED])
    host.active = 'longcat'
    const face = faceOf(host)
    await face.credentials.set('LONGCAT_API_KEY', 'k')
    const catalog = (await face.session.modelCatalog())['value'] as {
      groups: { models: { reasoning?: { defaultEffort?: string } }[] }[]
    }
    expect(catalog.groups[0]!.models[0]!.reasoning?.defaultEffort).toBe('high')

    // 声明里没有 high → 不给 defaultEffort（席位维持无预选），不发明档位
    const withoutHigh = fakeHost([LONGCAT_EFFORTS_NO_HIGH])
    withoutHigh.active = 'longcat'
    const face2 = faceOf(withoutHigh)
    await face2.credentials.set('LONGCAT_API_KEY', 'k')
    const catalog2 = (await face2.session.modelCatalog())['value'] as {
      groups: { models: { reasoning?: { defaultEffort?: string } }[] }[] }
    expect(catalog2.groups[0]!.models[0]!.reasoning?.defaultEffort).toBeUndefined()
  })

  it('未接线的配置域直接拒绝，不返回一个能写但没人读的空文档', async () => {
    const host = fakeHost([LONGCAT])
    const result = await faceOf(host).settings.mutate('ui-settings', [{ op: 'set', path: ['enabled'], value: true }], 0)
    expect(result['ok']).toBe(false)
    expect((result['error'] as { code: string }).code).toBe('settings/rejected')
    expect(host.calls.some(call => call.command === 'save_providers')).toBe(false)
  })

  it('首屏说明的确认写在 general 域，不动服务商文档', async () => {
    const host = fakeHost([LONGCAT])
    const result = await faceOf(host).settings.mutate('ui-settings-general', [
      { op: 'set', path: ['welcomeNoticeVersion'], value: '2026-09-28.1' },
    ], 0)
    expect(result['ok']).toBe(true)
    expect((result['value'] as { revision: number }).revision).toBe(1)
    expect((result['value'] as { value: Record<string, unknown> }).value['welcomeNoticeVersion']).toBe('2026-09-28.1')
    expect(host.calls.some(call => call.command === 'save_providers')).toBe(false)
  })
})

describe('权限挡位域（permission 命名空间，SH-105 补修）', () => {
  /** describe 里的 permission 视图。 */
  const permissionOf = async (face: LooseFace): Promise<Record<string, any>> => {
    const described = (await face.settings.describe())['value'] as { namespaces: Record<string, any>[] }
    return described.namespaces.find(ns => ns['ns'] === 'permission')!
  }

  it('describe 暴露 permission 命名空间：defaultPreset 反推自宿主沙箱档，schema 声明三档', async () => {
    const host = fakeHost([LONGCAT])
    host.sandboxMode = 'danger_full_access'
    const view = await permissionOf(faceOf(host))
    // 缺这个命名空间时 DSH 的挡位行会静默不写（settings-store.ts:109-116），
    // 连一句错误都显示不出来——这正是"点芯片没反应"的根因之一。
    expect(view).toBeDefined()
    expect(view['value']).toEqual({ defaultPreset: 'danger-full-access' })
    expect(view['applies']).toBe('live')
    expect(view['secrets']).toEqual([])
    expect(view['revision']).toBe(0)
    // schema 的枚举必须覆盖全集：界面按它渲染下拉，缺一档就少一个可选项
    const schema = JSON.stringify(view['schema'])
    for (const preset of ['read-only', 'workspace-write', 'danger-full-access']) {
      expect(schema, preset).toContain(preset)
    }
  })

  it('认不出的沙箱档回落最小档，不猜一个更宽松的档位', async () => {
    const host = fakeHost([LONGCAT])
    host.sandboxMode = 'some-future-mode'
    const view = await permissionOf(faceOf(host))
    expect(view['value']).toEqual({ defaultPreset: 'read-only' })
  })

  it('写 defaultPreset：落宿主 [sandbox] mode、刷新快照、修订号 +1 并广播', async () => {
    const host = fakeHost([LONGCAT])
    host.sandboxMode = 'read_only'
    const face = faceOf(host)
    const result = await face.settings.mutate('permission', [
      { op: 'set', path: ['defaultPreset'], value: 'workspace-write' },
    ], 0)
    expect(result['ok']).toBe(true)
    // 唯一的落点是宿主的 [sandbox] mode（save_settings），不是前端自己记一份。
    // 键名必须是 camelCase（Tauri 形参映射）：写 `base_url`/`workspace_root`/`sandbox_mode`
    // 会得到 `missing required key baseUrl`，宿主一次都不会落盘（D-08 的真机错误）。
    const write = host.calls.find(call => call.command === 'save_settings')
    expect(Object.keys(write?.args ?? {}).filter(key => key.includes('_'))).toEqual([])
    expect(write?.args).toEqual({
      baseUrl: 'https://api.longcat.chat/openai/v1',
      model: 'LongCat-2.5',
      workspaceRoot: 'W',
      sandboxMode: 'workspace_write',
    })
    // 应答里的值来自写后重读的宿主快照（回弹给界面的是真实生效档）
    expect((result['value'] as { value: { defaultPreset: string } }).value.defaultPreset).toBe('workspace-write')
    expect((result['value'] as { revision: number }).revision).toBe(1)
    expect(host.emitted).toEqual([{ name: 'settings/document-updated', args: ['permission', 1] }])
    // 下一次 describe 的修订号也要跟上，否则界面第二次点击会被自己的旧修订号挡成 conflict
    expect((await permissionOf(face))['revision']).toBe(1)
    expect((await permissionOf(face))['value']).toEqual({ defaultPreset: 'workspace-write' })
  })

  it('save_providers 顶层 camelCase、providers 元素内部仍是 snake_case（顶层转、元素内不转）', async () => {
    const host = fakeHost([LONGCAT])
    const result = await faceOf(host).settings.mutate('llm-pi-ai', [{
      op: 'set',
      path: ['providers', 'minimax'],
      value: profileValue({
        models: [{ id: 'MiniMax-M2', name: 'MiniMax M2', contextWindow: 200_000, maxTokens: 8_192, input: ['text'] }],
      }),
    }], 0)
    expect(result['ok']).toBe(true)
    const args = host.calls.find(call => call.command === 'save_providers')!.args!
    // 顶层：Tauri 只映射命令形参名
    expect(Object.keys(args).sort()).toEqual(['activeProvider', 'expectedRevision', 'providers'])
    // 元素内部：serde 结构体字段，落盘名保持 snake_case（04 §1.3）。
    // "顺手统一成 camelCase"会把文档写坏——这条断言就是拦这个的。
    const row = (args['providers'] as Record<string, unknown>[])[1]!
    expect(Object.keys(row).sort()).toEqual([
      'base_url', 'id', 'label', 'model', 'models', 'provider', 'reasoning_effort', 'secret_ref',
    ])
    expect((row['models'] as Record<string, unknown>[])[0]).toMatchObject({
      context_window: 200_000, max_tokens: 8_192,
    })
  })

  it('修订号对不上报 conflict 并停在写宿主之前', async () => {
    const host = fakeHost([LONGCAT])
    const result = await faceOf(host).settings.mutate('permission', [
      { op: 'set', path: ['defaultPreset'], value: 'read-only' },
    ], 9)
    expect(result['ok']).toBe(false)
    expect((result['error'] as { code: string }).code).toBe('settings/conflict')
    expect((result['error'] as { details: unknown }).details).toMatchObject({ ns: 'permission', expected: 9, actual: 0 })
    expect(host.calls.some(call => call.command === 'save_settings')).toBe(false)
  })

  it('未知挡位、未知路径、删除与多 op 一律如实拒绝，绝不静默忽略', async () => {
    const host = fakeHost([LONGCAT])
    const face = faceOf(host)
    const cases: [string, Record<string, unknown>[], string][] = [
      ['上游实验档 auto 不在目录里', [{ op: 'set', path: ['defaultPreset'], value: 'auto' }], '未知权限挡位'],
      ['非字符串挡位', [{ op: 'set', path: ['defaultPreset'], value: 3 }], '未知权限挡位'],
      ['未知字段', [{ op: 'set', path: ['approvalMode'], value: 'never' }], 'defaultPreset'],
      ['删除语义', [{ op: 'unset', path: ['defaultPreset'] }], '只能把挡位设为某一档'],
      ['多 op', [
        { op: 'set', path: ['defaultPreset'], value: 'read-only' },
        { op: 'set', path: ['defaultPreset'], value: 'danger-full-access' },
      ], '一次只接受一个写入'],
    ]
    for (const [label, ops, keyword] of cases) {
      const result = await face.settings.mutate('permission', ops, 0)
      expect(result['ok'], label).toBe(false)
      expect((result['error'] as { code: string }).code, label).toBe('settings/rejected')
      expect((result['error'] as Error).message, label).toContain(keyword)
    }
    // 一条都不该落到宿主写面：拒绝就是拒绝，不是"写一半"
    expect(host.calls.some(call => call.command === 'save_settings')).toBe(false)
    expect(host.emitted).toEqual([])
  })
})

describe('credentials 面', () => {
  it('已声明的路由按宿主回显报告，未声明的合法引用名回落到逐名查询', async () => {
    const host = fakeHost([LONGCAT])
    host.revision = 1
    await faceOf(host).credentials.set('LONGCAT_API_KEY', 'sk-test')
    const described = await faceOf(host).credentials.describe(['LONGCAT_API_KEY', 'MINIMAX_API_KEY'])
    expect(described['value']).toMatchObject({
      LONGCAT_API_KEY: { configured: true, source: 'shaniu-dpapi', writable: true },
    })
    expect((described['value'] as Record<string, { configured: boolean }>)['MINIMAX_API_KEY'].configured).toBe(false)
    expect(host.calls.filter(call => call.command === 'has_api_key').map(call => call.args?.provider)).toContain('MINIMAX_API_KEY')
  })

  it('语法外的引用名报未配置而不是报错：DSH 的凭据面就是这么定的', async () => {
    const host = fakeHost([])
    const described = await faceOf(host).credentials.describe(['openai-responses', 'BAD-REF'])
    const info = described['value'] as Record<string, { configured: boolean; writable: boolean }>
    expect(info['openai-responses']).toEqual({ configured: false, writable: false })
    expect(info['BAD-REF']).toEqual({ configured: false, writable: false })
    // 语法外的名字在 DPAPI 里会被清洗成另一个文件，问了也问不到真话
    expect(host.calls.some(call => call.command === 'has_api_key')).toBe(false)
  })

  it('空密钥不写入；写入与删除都通知引用已变', async () => {
    const host = fakeHost([LONGCAT])
    const face = faceOf(host)
    const empty = await face.credentials.set('LONGCAT_API_KEY', '')
    expect(empty['ok']).toBe(false)
    expect((empty['error'] as { code: string }).code).toBe('credential/rejected')
    await face.credentials.set('MINIMAX_API_KEY', 'sk-m')
    await face.credentials.unset('MINIMAX_API_KEY')
    expect(host.emitted.map(event => event.name)).toEqual([
      'credentials/reference-updated', 'llm/adapters-updated',
      'credentials/reference-updated', 'llm/adapters-updated',
    ])
    const stored = host.calls.find(call => call.command === 'store_api_key')
    expect(stored?.args).toEqual({ provider: 'MINIMAX_API_KEY', key: 'sk-m' })
    expect(host.calls.find(call => call.command === 'delete_api_key')?.args).toEqual({ provider: 'MINIMAX_API_KEY' })
  })

  it('一次描述超过 64 个引用按拒绝处理（设置页自己会分片）', async () => {
    const face = faceOf(fakeHost([]))
    const refs = Array.from({ length: 65 }, (_, index) => `KEY_${index}`)
    const result = await face.credentials.describe(refs)
    expect(result['ok']).toBe(false)
    expect((result['error'] as Error).message).toContain('64')
  })
})

describe('llm 面', () => {
  it('目录项与文档里的路由合成一份可配置清单：命中目录仍算内建，自定义算手声明', async () => {
    const host = fakeHost([LONGCAT, { ...LONGCAT, id: 'openai', label: '', base_url: '', model: '', secret_ref: 'OPENAI_API_KEY', models: [] }])
    const listed = await faceOf(host).llm.listConfigurableProviders()
    const entries = listed['value'] as { provider: string; settingsNs: string; settingsPath: string[]; declared?: boolean; displayName: string }[]
    const byProvider = new Map(entries.map(entry => [entry.provider, entry]))
    expect(byProvider.get('openai')).toMatchObject({ declared: false, displayName: 'OpenAI', settingsNs: 'llm-pi-ai', settingsPath: ['providers', 'openai'] })
    expect(byProvider.get('longcat')).toMatchObject({ declared: true, displayName: 'LongCat' })
    // 未声明 nativeAuth：后端不读宿主环境变量，报了就是给界面一个假的"已就绪"来源
    expect(byProvider.get('anthropic')).not.toHaveProperty('nativeAuth')
  })

  it('目录内 provider 直接答自己的模型；自建端点转宿主命令（SH-098），失败包 llm/model-discovery-rejected', async () => {
    const face = faceOf(fakeHost([]))
    const known = await face.llm.discoverModels('llm-pi-ai', { provider: 'zai' })
    expect((known['value'] as { id: string }[]).map(model => model.id)).toContain('glm-5.3')
    const wrongNs = await face.llm.discoverModels('llm-deepseek', { provider: 'deepseek' })
    expect(wrongNs['ok']).toBe(false)
  })

  it('自建端点模型发现（SH-098）：探针密钥透传宿主，失败按 llm/model-discovery-rejected 回显', async () => {
    const host = fakeHost([])
    const face = faceOf(host)
    // 成功：走宿主命令，探针密钥原样透传（宿主优先用表单未存的 Key）
    const found = await face.llm.discoverModels('llm-pi-ai', {
      baseURL: 'https://api.example.com/v1', api: 'openai-completions', apiKey: 'probe-key',
    })
    expect(found['ok']).toBe(true)
    expect((found['value'] as { id: string }[]).map(model => model.id)).toEqual(['m-a', 'm-b'])
    const call = host.calls.find(entry => entry.command === 'discover_models')
    expect(call?.args).toEqual({ provider: '', baseUrl: 'https://api.example.com/v1', apiKey: 'probe-key' })

    // 失败：宿主消息按原码回显，绝不被吞成空列表
    host.discoverError = 'https://api.example.com/v1/models 返回 401（请检查 API Key 是否正确）'
    const failed = await face.llm.discoverModels('llm-pi-ai', { baseURL: 'https://api.example.com/v1' })
    expect(failed['ok']).toBe(false)
    expect((failed['error'] as { code: string }).code).toBe('llm/model-discovery-rejected')
    expect((failed['error'] as Error).message).toContain('401')

    // 空端点：不 invoke（宿主一次网络面都不开）
    const calls = host.calls.filter(entry => entry.command === 'discover_models').length
    const blank = await face.llm.discoverModels('llm-pi-ai', {})
    expect(blank['ok']).toBe(false)
    expect((blank['error'] as Error).message).toContain('端点地址为空')
    expect(host.calls.filter(entry => entry.command === 'discover_models').length).toBe(calls)

    // 不支持的协议：上游目录之外的 api 拒绝
    const unsupported = await face.llm.discoverModels('llm-pi-ai', {
      baseURL: 'https://x/v1', api: 'anthropic-messages',
    })
    expect(unsupported['ok']).toBe(false)
    expect((unsupported['error'] as Error).message).toContain('OpenAI 兼容端点')
  })

  it('listProviders 只列真能跑的，名字取展示名并回落目录名', async () => {
    const host = fakeHost([LONGCAT])
    await faceOf(host).credentials.set('LONGCAT_API_KEY', 'sk')
    const listed = await faceOf(host).llm.listProviders()
    expect(listed['value']).toEqual([{ id: 'longcat', name: 'LongCat' }])

    const noKey = await faceOf(fakeHost([LONGCAT])).llm.listProviders()
    expect(noKey['value']).toEqual([])
  })
})

describe('席位的生效档', () => {
  it('切换服务商写整份文档并保持其它家不动', async () => {
    const other: Row = { ...LONGCAT, id: 'other', label: 'Other', secret_ref: 'OTHER_API_KEY', model: 'o-1', models: [{ id: 'o-1', name: 'o-1', context_window: 1000, max_tokens: 100, input: [] }] }
    const host = fakeHost([LONGCAT, other])
    host.active = 'longcat'
    const face = faceOf(host)
    await face.credentials.set('LONGCAT_API_KEY', 'sk')
    await face.credentials.set('OTHER_API_KEY', 'sk')
    const result = await face.session.selectModel({ provider: 'other', model: 'o-1' })
    expect(result['value']).toEqual({ selected: { provider: 'other', model: 'o-1' } })
    expect(host.active).toBe('other')
    expect(host.rows().map(row => row.model)).toEqual(['LongCat-2.5', 'o-1'])
    expect(face.activeSelection()).toEqual({ provider: 'other', model: 'o-1' })
  })

  it('读不到宿主文档时拒绝切换，不发半份文档', async () => {
    const host = fakeHost([LONGCAT])
    host.failRead = true
    const face = faceOf(host)
    const result = await face.session.selectModel({ provider: 'longcat', model: 'LongCat-2.5' })
    expect(result['ok']).toBe(false)
    expect(host.calls.some(call => call.command === 'save_providers')).toBe(false)
  })

  it('没配密钥的档位不进目录，activeSelection 留空而不是显示一个跑不通的模型', async () => {
    const face = faceOf(fakeHost([LONGCAT]))
    expect(face.activeSelection()).toEqual({ provider: '', model: '' })
    const catalog = await face.session.modelCatalog()
    expect((catalog['value'] as { failures: { id: string }[] }).failures.map(item => item.id)).toEqual(['longcat'])
  })
})
