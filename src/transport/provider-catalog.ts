/**
 * 第三方服务商目录：mock 联调面与真实宿主面共用的一份数据。
 *
 * 为什么必须共用：两处各存一份就会漂移，而漂移的表现形式是"设置页里这家公司有
 * 五个模型、席位里只有两个"这种无法复现的显示差。id / 模型名 / 上下文 / 输出上限
 * 逐字取自官方 `@earendil-works/pi-ai@1.3.0` 的模型数据，端点与密钥环境变量名取自
 * DSH 的 `provider-data.generated.ts`。
 *
 * 收录的是 SHANIU 常用的七家，不是完整目录（官方 41 家）：完整目录属于后端适配器
 * 的接入面，前端只列目录并不会让那些端点变得可用。
 */

/** 目录里一行的模型（`reasoning` 只服务显示：后端未接推理档，见 SH-096）。 */
export interface CatalogModelRow {
  readonly id: string
  readonly name: string
  readonly contextWindow: number
  readonly maxTokens: number
  readonly reasoning: boolean
  /** 输入模态；缺省 = 仅 text（模型发现回显用）。 */
  readonly input?: readonly ('text' | 'image')[]
}

/** 一个内建服务商（官方 llm-pi-ai 目录项的精简面）。 */
export interface CatalogProviderEntry {
  readonly displayName: string
  readonly defaultBaseURL: string
  /** 适配器原生读取的密钥环境变量（第一个作为首选提示）。 */
  readonly env: readonly string[]
  readonly models: readonly CatalogModelRow[]
  /**
   * 展示次序：越小越靠前；缺省排在内置之后。排序在数据层显式声明，
   * 不在界面里按数组下标或 id 特判置顶。
   */
  readonly priority?: number
  /** 是否为 SHANIU 出厂内置服务商（区别于「目录里可添加的第三方」）。 */
  readonly builtin?: boolean
}

export const PROVIDER_CATALOG: Record<string, CatalogProviderEntry> = {
  deepseek: {
    displayName: 'DeepSeek',
    defaultBaseURL: 'https://api.deepseek.com',
    env: ['DEEPSEEK_API_KEY'],
    // 与 Rust `config/builtin_providers.rs`、`resources/model-catalog.json`
    // （2026-11-09 端点实测）保持同一份数值。
    priority: 0,
    builtin: true,
    models: [
      { id: 'deepseek-flash', name: 'DeepSeek-V4.1-Flash', contextWindow: 1_048_576, maxTokens: 393_216, reasoning: true, input: ['text', 'image'] },
      { id: 'deepseek-v4-pro', name: 'DeepSeek-V4-Pro', contextWindow: 1_048_576, maxTokens: 393_216, reasoning: true },
    ],
  },
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

/** 目录路由的密钥引用名：与后端 `ProviderProfile::secret_name()`、设置页 `deriveKeyRef` 同一条规则。 */
export function deriveKeyRef(route: string): string {
  return `${route.toUpperCase().replace(/[^A-Z0-9]+/g, '_')}_API_KEY`
}
