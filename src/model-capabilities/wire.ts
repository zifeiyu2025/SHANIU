/**
 * 能力视图的数据面：只读 `get_settings`（视图字段，不属于 config.toml）。
 *
 * 字段名跟着宿主 serde 的 camelCase 走（与 settings-face.ts 同源），这里只挑
 * 能力区块需要的键——不复制整份设置模型，避免上游加字段时跟着漂移。
 */
import { tauriBridge, type HostBridge } from '../transport/tauri'

/** 一项能力的视图（`capability::model_view` 的条目形状）。 */
export interface CapabilityViewWire {
  /** 布尔项为 true/false，数值项为数字，档位项没有该字段。 */
  readonly value?: boolean | number
  /** 声明档位（档位项）。 */
  readonly levels?: readonly string[]
  /** 实测通过的档位（档位项）。 */
  readonly verifiedLevels?: readonly string[]
  /** 默认档（档位项）。 */
  readonly default?: string
  /** unknown / endpoint / builtin / inferred / probe / manual。 */
  readonly source?: string
  /** verified / mismatch / probing / unverified / unjudgeable。 */
  readonly state?: string
  /** 实测证据原文（"红图→红色"这类），悬浮可见。 */
  readonly evidence?: string
  /** 内置表数据日期（毫秒）；只有内置表/推测来源才带。 */
  readonly catalogUpdatedAt?: number
  readonly checkedAtMs?: number
}

/** 一行模型的能力视图。 */
export interface CapabilityModelWire {
  readonly id: string
  readonly name?: string
  readonly contextWindow?: number
  readonly maxTokens?: number
  readonly input?: readonly string[]
  readonly capabilities?: Readonly<Record<string, CapabilityViewWire>>
}

/** 一家服务商在设置文档里的模型清单。 */
export interface CapabilityProviderWire {
  readonly id: string
  readonly label?: string
  readonly models: readonly CapabilityModelWire[]
}

/** 一次密钥校验的结果（`check_api_key` 的 wire 形状，A0-1）。 */
export interface KeyCheckWire {
  /** valid / invalid / unknown（unknown = 判断不了，不等于无效）。 */
  readonly state: 'valid' | 'invalid' | 'unknown'
  /** ok / key_invalid / missing / network / timeout / endpoint_unsupported / rate_limited / server / quota / bad_request / storage / no_endpoint / unknown */
  readonly reason: string
  /** 一句人话。 */
  readonly message: string
  readonly httpStatus?: number
  readonly providerCode?: string
  readonly providerMessage?: string
  readonly latencyMs?: number
}

/** 能力区块的数据面；测试注入脚本化实现，运行时走 `hostCapabilitiesWire()`。 */
export interface CapabilitiesWire {
  providers(): Promise<readonly CapabilityProviderWire[]>
  /**
   * 手动重探（`probe_capabilities(force=true)`）。「不可判」是终态、不会自动重试，
   * 这是它唯一的出口；可选是为了让只读场景（测试夹具）不必实现它。
   */
  reprobe?(provider: string, model: string): Promise<void>
  /**
   * 密钥三态校验（`check_api_key`，打 `GET /models`，不产生推理费用）。
   * 不做缓存：保存/更换密钥后必须重新问端点，吃旧结论会把配错显示成配好。
   */
  checkKey?(provider: string): Promise<KeyCheckWire>
}

/** 桌面运行时实现（命令名与 Rust `generate_handler!` 逐字对应）。 */
export function hostCapabilitiesWire(bridge: HostBridge = tauriBridge()): CapabilitiesWire {
  return {
    providers: async () => {
      const snapshot = await bridge.invoke<{ providers?: readonly CapabilityProviderWire[] }>('get_settings')
      return snapshot.providers ?? []
    },
    reprobe: async (provider, model) => {
      // 先作废这一条缓存再重探；后台任务不等结果，轮询自然会把「检测中」翻过去。
      await bridge.invoke('probe_capabilities', { provider, model, force: true })
    },
    checkKey: async (provider) => bridge.invoke<KeyCheckWire>('check_api_key', { provider }),
  }
}
