/**
 * mock-models 的行为契约测试。
 * 断言对象是「模型设置」页实际读到的东西：它自己的 schema 算子能否导航这份信封、
 * 写路径是否回视图与新 revision、席位目录是否与设置页同源、密钥是否只以状态出现。
 */
import { beforeEach, describe, expect, it, vi } from 'vitest'
import { Context } from '@deepseek-ai/cordis'
import { SettingsSchemaService } from '../dsh/packages/client/ui-settings/src/client/schema.ts'
import { createSettingsSchemaOperations } from '../dsh/packages/client/ui-settings-models/src/client/schema-operations.ts'
import { createModelsMock, type ModelsMock } from './mock-models.ts'

const schema = createSettingsSchemaOperations(new SettingsSchemaService(new Context()))

/** 设置页读某个 namespace 视图的便捷面。 */
async function viewOf(mock: ModelsMock, ns: string) {
  const described = await mock.settings.describe()
  if (!described.ok) throw new Error('describe failed')
  const view = described.value.namespaces.find(candidate => candidate.ns === ns)
  if (view === undefined) throw new Error(`namespace ${ns} missing`)
  return view
}

/** 一个自定义添加卡片会写入的手声明 profile（acme-1 会推理，acme-2 不声明，用于对照）。 */
const acmeProfile = {
  displayName: 'Acme Gateway',
  apiKeyEnv: 'ACME_API_KEY',
  api: 'openai-responses',
  baseURL: 'https://acme.invalid/v1',
  models: [
    {
      id: 'acme-1', name: 'Acme 1', contextWindow: 128_000,
      reasoningEfforts: { off: null, low: 'low', medium: 'medium', high: 'high', max: 'max' },
    },
    { id: 'acme-2', name: 'Acme 2', contextWindow: 64_000, reasoningEfforts: false },
  ],
}

/** 出厂没有自定义服务商：需要自定义路由的用例走界面同款写路径现场添加，再配密钥。 */
async function withCustomProvider<T>(mock: ModelsMock, run: () => Promise<T>): Promise<T> {
  const view = await viewOf(mock, 'llm-pi-ai')
  const written = await mock.settings.mutate(
    'llm-pi-ai', [{ op: 'set', path: ['providers', 'acme'], value: acmeProfile }], view.revision,
  )
  if (!written.ok) throw new Error(`写入自定义服务商失败：${written.error.message}`)
  await mock.credentials.set('ACME_API_KEY', 'sk-mock')
  try {
    return await run()
  } finally {
    await mock.credentials.unset('ACME_API_KEY')
  }
}

describe('mock-models：设置页的可读基线', () => {
  let mock: ModelsMock
  beforeEach(() => {
    mock = createModelsMock()
  })

  it('describe 报告可写，并挂出页面所需的全部 namespace', async () => {
    const described = await mock.settings.describe()
    expect(described.ok).toBe(true)
    if (!described.ok) return
    expect(described.value.writable).toBe(true)
    const namespaces = described.value.namespaces.map(view => view.ns)
    // 页面：目录行 + 自定义添加；设置-常规：developer tools 与 welcome notice
    expect(namespaces).toEqual(expect.arrayContaining([
      'llm-deepseek', 'llm-pi-ai', 'ui-settings', 'ui-settings-general',
    ]))
    for (const view of described.value.namespaces) {
      expect(view.applies).toBe('live')
      expect(typeof view.revision).toBe('number')
    }
  })

  it('llm-pi-ai 的 schema 信封可被页面自己的算子读出协议与推理档位', async () => {
    const view = await viewOf(mock, 'llm-pi-ai')
    const api = schema.nodeAtPath(schema.rehydrate(view.schema), ['providers', '\u0000probe', 'api']) as
      { type?: string; list?: { value?: unknown }[] } | undefined
    // 「自定义添加」是否可用，取决于这里能否读出非空 union
    expect(api?.type).toBe('union')
    expect((api?.list ?? []).length).toBeGreaterThan(0)
    const efforts = schema.nodeAtPath(schema.rehydrate(view.schema), ['providers', '\u0000probe', 'models', '0', 'reasoningEfforts']) as
      { list?: readonly { type?: string; sKey?: { list?: { value?: unknown }[] } }[] } | undefined
    // reasoningLevelChoices 取的是 union 里那个 dict 分支的键枚举
    const dictBranch = (efforts?.list ?? []).find(branch => branch.type === 'dict')
    const levels = (dictBranch?.sKey?.list ?? []).map(entry => entry.value)
    expect(levels).toEqual(expect.arrayContaining(['off', 'low', 'medium', 'high']))
  })

  it('出厂没有自定义服务商：providers 为空，目录只列内建服务商', async () => {
    const view = await viewOf(mock, 'llm-pi-ai')
    expect(schema.getPath(view.value, ['providers'])).toEqual({})
    const declared = await mock.llm.listConfigurableProviders()
    if (!declared.ok) return
    expect(declared.value.filter(entry => entry.declared === true)).toEqual([])
    expect(declared.value.map(entry => entry.provider)).toEqual(expect.arrayContaining(['deepseek', 'openai', 'anthropic']))
  })

  it('预置的 DeepSeek 行有密钥引用但没有密钥（页面据此画红点）', async () => {
    const view = await viewOf(mock, 'llm-deepseek')
    expect(schema.getPath(view.value, ['apiKeyEnv'])).toBe('DEEPSEEK_API_KEY')
    const described = await mock.credentials.describe(['DEEPSEEK_API_KEY'])
    expect(described.ok).toBe(true)
    if (!described.ok) return
    expect(described.value.DEEPSEEK_API_KEY?.configured).toBe(false)
  })
})

describe('mock-models：添加服务商这条写路径', () => {
  let mock: ModelsMock
  beforeEach(() => {
    mock = createModelsMock()
  })

  it('写入 providers.<route> 后回新视图，目录里随即出现该条目', async () => {
    const before = await viewOf(mock, 'llm-pi-ai')
    const written = await mock.settings.mutate('llm-pi-ai', [{ op: 'set', path: ['providers', 'acme'], value: acmeProfile }], before.revision)
    expect(written.ok).toBe(true)
    if (!written.ok) return
    expect(written.value.revision).toBe(before.revision + 1)
    expect(schema.getPath(written.value.user, ['providers', 'acme'])).toEqual(acmeProfile)
    expect(schema.getPath(written.value.value, ['providers', 'acme', 'baseURL'])).toBe('https://acme.invalid/v1')

    const declared = await mock.llm.listConfigurableProviders()
    expect(declared.ok).toBe(true)
    if (!declared.ok) return
    const entry = declared.value.find(candidate => candidate.provider === 'acme')
    expect(entry).toMatchObject({ settingsNs: 'llm-pi-ai', settingsPath: ['providers', 'acme'], declared: true })
  })

  it('拿旧 revision 写入是冲突而非覆盖', async () => {
    const before = await viewOf(mock, 'llm-pi-ai')
    const stale = await mock.settings.mutate('llm-pi-ai', [{ op: 'set', path: ['providers', 'acme'], value: acmeProfile }], before.revision + 5)
    expect(stale.ok).toBe(false)
    if (stale.ok) return
    expect(stale.error.code).toBe('settings/conflict')
    const after = await viewOf(mock, 'llm-pi-ai')
    expect(schema.getPath(after.user, ['providers', 'acme'])).toBeUndefined()
    expect(after.revision).toBe(before.revision)
  })

  it('未知 namespace 的写入被拒绝', async () => {
    const refused = await mock.settings.mutate('llm-nope', [{ op: 'set', path: ['x'], value: 1 }], undefined)
    expect(refused.ok).toBe(false)
    if (refused.ok) return
    expect(refused.error.code).toBe('settings/rejected')
  })

  it('unset 能整条撤掉一个手声明路由', async () => {
    const before = await viewOf(mock, 'llm-pi-ai')
    await mock.settings.mutate('llm-pi-ai', [{ op: 'set', path: ['providers', 'acme'], value: acmeProfile }], before.revision)
    const mid = await viewOf(mock, 'llm-pi-ai')
    const removed = await mock.settings.mutate('llm-pi-ai', [{ op: 'unset', path: ['providers', 'acme'] }], mid.revision)
    expect(removed.ok).toBe(true)
    if (!removed.ok) return
    expect(schema.getPath(removed.ok ? removed.value.user : {}, ['providers', 'acme'])).toBeUndefined()
  })
})

describe('mock-models：写入后的失效推送', () => {
  it('settings 写入推送 document-updated，凭据写入推送 reference-updated', async () => {
    const events: [string, unknown][] = []
    const mock = createModelsMock({ emit: (name, payload) => { events.push([name, payload]); return true } })
    const before = await viewOf(mock, 'llm-pi-ai')
    await mock.settings.mutate('llm-pi-ai', [{ op: 'set', path: ['providers', 'acme'], value: acmeProfile }], before.revision)
    expect(events.some(([name, payload]) => name === 'settings/document-updated'
      && Array.isArray(payload) && payload[0] === 'llm-pi-ai')).toBe(true)

    events.length = 0
    await mock.credentials.set('ACME_API_KEY', 'sk-acme')
    expect(events.map(([name]) => name)).toContain('credentials/reference-updated')

    events.length = 0
    await mock.credentials.unset('ACME_API_KEY')
    expect(events.map(([name]) => name)).toContain('credentials/reference-updated')
  })

  it('席位目录随推送刷新：新服务商配好密钥后可路由', async () => {
    const mock = createModelsMock({ emit: () => true })
    const before = await viewOf(mock, 'llm-pi-ai')
    await mock.settings.mutate('llm-pi-ai', [{ op: 'set', path: ['providers', 'acme'], value: acmeProfile }], before.revision)
    await mock.credentials.set('ACME_API_KEY', 'sk-acme')
    const catalog = await mock.session.modelCatalog()
    expect(catalog.ok && catalog.value.routableProviders).toContain('acme')
  })
})

describe('mock-models：凭据只以状态出现', () => {
  let mock: ModelsMock
  beforeEach(() => {
    mock = createModelsMock()
  })

  it('存取翻转 configured，且任何返回值都不携带密钥字面量', async () => {
    const stored = await mock.credentials.set('ACME_API_KEY', 'sk-secret-literal')
    expect(stored.ok).toBe(true)
    const described = await mock.credentials.describe(['ACME_API_KEY'])
    expect(described.ok).toBe(true)
    if (!described.ok) return
    expect(described.value.ACME_API_KEY).toMatchObject({ configured: true, writable: true })
    expect(JSON.stringify(described.value)).not.toContain('sk-secret-literal')

    await mock.credentials.unset('ACME_API_KEY')
    const after = await mock.credentials.describe(['ACME_API_KEY'])
    expect(after.ok && after.value.ACME_API_KEY?.configured).toBe(false)
  })

  it('非法引用名被拒绝（引用要同时当 settings 键与 shell 标识符用）', async () => {
    expect((await mock.credentials.set('1starts-with-digit', 'x')).ok).toBe(false)
    expect((await mock.credentials.set('has-dash', 'x')).ok).toBe(false)
  })

  it('describe 超过 64 个引用被拒绝，而不是静默截断', async () => {
    const refs = Array.from({ length: 65 }, (_, index) => `REF_${index}`)
    const result = await mock.credentials.describe(refs)
    expect(result.ok).toBe(false)
  })
})

describe('mock-models：模型席位与设置页同源', () => {
  let mock: ModelsMock
  beforeEach(() => {
    mock = createModelsMock()
  })

  it('未配密钥的服务商不进席位目录，配了才进', async () => {
    const before = await viewOf(mock, 'llm-pi-ai')
    await mock.settings.mutate('llm-pi-ai', [{ op: 'set', path: ['providers', 'acme'], value: acmeProfile }], before.revision)
    const beforeKey = await mock.session.modelCatalog()
    expect(beforeKey.ok).toBe(true)
    if (!beforeKey.ok) return
    expect(beforeKey.value.groups.map(group => group.id)).not.toContain('acme')

    await mock.credentials.set('ACME_API_KEY', 'sk-acme')
    const afterKey = await mock.session.modelCatalog()
    expect(afterKey.ok).toBe(true)
    if (!afterKey.ok) return
    const group = afterKey.value.groups.find(candidate => candidate.id === 'acme')
    expect(group?.models.map(model => model.id)).toContain('acme-1')
    expect(afterKey.value.routableProviders).toContain('acme')
  })

  it('出厂未配置：目录为空，席位默认项是空选择', async () => {
    const catalog = await mock.session.modelCatalog()
    expect(catalog.ok).toBe(true)
    if (!catalog.ok) return
    expect(catalog.value.groups).toEqual([])
    expect(catalog.value.default).toEqual({ provider: '', model: '' })
  })

  it('席位默认项始终落在目录里，避免既有选择退化', async () => {
    await withCustomProvider(mock, async () => {
      await mock.session.selectModel({ provider: 'acme', model: 'acme-1' })
      const catalog = await mock.session.modelCatalog()
      expect(catalog.ok).toBe(true)
      if (!catalog.ok) return
      const group = catalog.value.groups.find(candidate => candidate.id === catalog.value.default.provider)
      expect(group?.models.map(model => model.id)).toContain(catalog.value.default.model)
    })
  })

  it('账号提供商已彻底移除：设置目录与席位分组都不再有 deepseek-account', async () => {
    const directory = await mock.llm.listConfigurableProviders()
    expect(directory.ok && directory.value.some(entry => entry.provider === 'deepseek-account')).toBe(false)
    const catalog = await mock.session.modelCatalog()
    expect(catalog.ok && catalog.value.groups.some(group => group.id === 'deepseek-account')).toBe(false)
  })

  it('discoverModels 走本地目录，不发任何网络请求', async () => {
    const fetchSpy = vi.fn()
    globalThis.fetch = fetchSpy
    const found = await mock.llm.discoverModels('llm-pi-ai', { baseURL: 'https://acme.invalid/v1', api: 'openai-responses' })
    expect(found.ok).toBe(true)
    if (!found.ok) return
    expect(found.value.length).toBeGreaterThan(0)
    expect(found.value.every(model => typeof model.id === 'string')).toBe(true)
    expect(fetchSpy).not.toHaveBeenCalled()
  })

  it('listProviders 只报可路由的路由', async () => {
    await withCustomProvider(mock, async () => {
      const providers = await mock.llm.listProviders()
      expect(providers.ok).toBe(true)
      if (!providers.ok) return
      const ids = providers.value.map(provider => provider.id)
      expect(ids).toContain('acme')
      expect(ids).not.toContain('deepseek')
    })
  })
})

describe('mock-models：模型的推理等级', () => {
  let mock: ModelsMock
  beforeEach(() => {
    mock = createModelsMock()
  })

  /** 取某个分组里的某个模型。 */
  async function modelOf(provider: string, model: string) {
    const catalog = await mock.session.modelCatalog()
    if (!catalog.ok) throw new Error('catalog failed')
    return catalog.value.groups.find(group => group.id === provider)?.models.find(entry => entry.id === model)
  }

  it('声明了 reasoningEfforts 的模型带档位，没声明的模型没有 reasoning', async () => {
    await withCustomProvider(mock, async () => {
      const reasoningModel = await modelOf('acme', 'acme-1')
      expect(reasoningModel?.reasoning?.efforts.map(effort => effort.id)).toEqual(['off', 'low', 'medium', 'high', 'max'])
      expect(reasoningModel?.reasoning?.defaultEffort).toBe('high')
      const plainModel = await modelOf('acme', 'acme-2')
      expect(plainModel?.reasoning).toBeUndefined()
    })
  })

  it('出厂 DeepSeek 模型带上游硬编码的 off/low/high/max 档位', async () => {
    // DeepSeek 分组要等密钥就绪才进目录，这里临时配上再收回，别把默认演示态改掉
    await mock.credentials.set('DEEPSEEK_API_KEY', 'sk-test-only')
    try {
      const model = await modelOf('deepseek', 'deepseek-v4-flash')
      expect(model?.reasoning?.efforts.map(effort => effort.id)).toEqual(['off', 'low', 'high', 'max'])
      expect(model?.reasoning?.defaultEffort).toBe('high')
    } finally {
      await mock.credentials.unset('DEEPSEEK_API_KEY')
    }
  })

  it('席位默认项带上有效档位；换模型后旧档不跟着串台', async () => {
    await withCustomProvider(mock, async () => {
      await mock.session.selectModel({ provider: 'acme', model: 'acme-1', reasoningEffort: 'low' })
      const kept = await mock.session.modelCatalog()
      expect(kept.ok && kept.value.default).toMatchObject({ model: 'acme-1', reasoningEffort: 'low' })

      // acme-2 没有推理档：旧档必须作废，而不是把 'low' 带过去
      await mock.session.selectModel({ provider: 'acme', model: 'acme-2' })
      const switched = await mock.session.modelCatalog()
      expect(switched.ok && switched.value.default).toMatchObject({ model: 'acme-2' })
      expect(switched.ok && switched.value.default.reasoningEffort).toBeUndefined()
    })
  })

  it('给不支持该档的模型强塞 effort 时回落到它自己的默认档', async () => {
    await withCustomProvider(mock, async () => {
      await mock.session.selectModel({ provider: 'acme', model: 'acme-1', reasoningEffort: 'xhigh' })
      const catalog = await mock.session.modelCatalog()
      expect(catalog.ok && catalog.value.default.reasoningEffort).toBe('high')
    })
  })
})

describe('mock-models：权限默认档设置', () => {
  it('挂出 permission 命名空间，默认档是 workspace-write', async () => {
    const mock = createModelsMock()
    const view = await viewOf(mock, 'permission')
    expect(schema.getPath(view.value, ['defaultPreset'])).toBe('workspace-write')
    expect(mock.permissionDefault()).toBe('workspace-write')
  })

  it('写入 defaultPreset 后视图与访问器一起更新', async () => {
    const mock = createModelsMock()
    const before = await viewOf(mock, 'permission')
    const written = await mock.settings.mutate('permission', [
      { op: 'set', path: ['defaultPreset'], value: 'workspace-write' },
    ], before.revision)
    expect(written.ok).toBe(true)
    if (!written.ok) return
    expect(schema.getPath(written.value.user, ['defaultPreset'])).toBe('workspace-write')
    expect(mock.permissionDefault()).toBe('workspace-write')
  })
})

describe('mock-models：第三方服务商目录', () => {
  it('目录列出内建服务商；发现模型直接答本地目录、不发请求', async () => {
    const mock = createModelsMock()
    const directory = await mock.llm.listConfigurableProviders()
    if (!directory.ok) throw new Error('directory failed')
    expect(directory.value.find(entry => entry.provider === 'openai')).toMatchObject({
      displayName: 'OpenAI',
      settingsNs: 'llm-pi-ai',
      settingsPath: ['providers', 'openai'],
      declared: false,
      defaultBaseURL: 'https://api.openai.com/v1',
      nativeAuth: { env: ['OPENAI_API_KEY'], oauth: false },
    })

    const fetchSpy = vi.fn()
    globalThis.fetch = fetchSpy
    // 内建服务商既不需要端点，也不触网（与官方适配器「答自己的模型」同口径）
    const found = await mock.llm.discoverModels('llm-pi-ai', { provider: 'openai' })
    expect(found.ok).toBe(true)
    if (!found.ok) return
    expect(found.value.map(model => model.id)).toEqual(expect.arrayContaining(['gpt-6.1-sol', 'o4-mini']))
    expect(fetchSpy).not.toHaveBeenCalled()
  })

  it('添加内建服务商：只写 apiKeyEnv，配上密钥后模型来自内建目录', async () => {
    const mock = createModelsMock()
    const view = await viewOf(mock, 'llm-pi-ai')
    await mock.settings.mutate('llm-pi-ai', [
      { op: 'set', path: ['providers', 'openai'], value: { apiKeyEnv: 'OPENAI_API_KEY' } },
    ], view.revision)

    // 未配密钥：不进席位目录
    const before = await mock.session.modelCatalog()
    expect(before.ok && before.value.groups.map(group => group.id)).not.toContain('openai')

    await mock.credentials.set('OPENAI_API_KEY', 'sk-openai-test')
    try {
      const after = await mock.session.modelCatalog()
      if (!after.ok) throw new Error('catalog failed')
      const group = after.value.groups.find(candidate => candidate.id === 'openai')
      expect(group?.name).toBe('OpenAI')
      const model = group?.models.find(entry => entry.id === 'gpt-6.1-sol')
      expect(model?.reasoning?.efforts.map(effort => effort.id)).toEqual(['off', 'low', 'medium', 'high', 'max'])
    } finally {
      await mock.credentials.unset('OPENAI_API_KEY')
    }
  })

  it('内建服务商被用户覆盖字段后仍算内建（declared=false）', async () => {
    const mock = createModelsMock()
    const view = await viewOf(mock, 'llm-pi-ai')
    await mock.settings.mutate('llm-pi-ai', [
      { op: 'set', path: ['providers', 'openai'], value: { displayName: 'My OpenAI', apiKeyEnv: 'OPENAI_API_KEY' } },
    ], view.revision)
    const directory = await mock.llm.listConfigurableProviders()
    if (!directory.ok) throw new Error('directory failed')
    expect(directory.value.find(entry => entry.provider === 'openai')).toMatchObject({
      displayName: 'My OpenAI',
      declared: false,
      defaultBaseURL: 'https://api.openai.com/v1',
    })
  })
})
