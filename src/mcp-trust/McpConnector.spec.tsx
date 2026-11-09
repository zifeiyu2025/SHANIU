// @vitest-environment jsdom
/**
 * MCP 连接器「出错时界面必须说人话」的契约测试。
 *
 * 第一批用户最可能卡在两处，而这两处以前只有状态词或只在日志里：
 *  1. `blocked`（例如 `node.exe` 装在用户改不了权限的目录）：必须同时露出
 *     **原因**与**可行做法**，不能只显示「已阻止」；
 *  2. 启动时 `config.toml` 解析失败 → 整份配置只读：界面必须明说
 *     「配置读取失败，已按只读运行」，否则用户会以为配置里的 server 消失了。
 *
 * 只测可见文案与数据来源，不改机制：这里不验证授权流程本身，
 * 容器隔离与 fail-closed 由 Rust 侧探针测试负责。
 */
import { beforeEach, describe, expect, it, vi } from 'vitest'
import { act, render, screen } from '@testing-library/react'
import type { McpServerSummaryWire } from '../transport/mcp-wire'
import { GeneralConfigReadOnlyRow } from '../config-readonly/ConfigReadOnly'
import { McpConnector } from './McpConnector'
import { zh, type McpTrustKey } from './locales'

const state = vi.hoisted(() => ({
  servers: [] as McpServerSummaryWire[],
  settings: { configReadOnly: null as string | null },
}))

vi.mock('../transport/tauri', () => ({
  tauriBridge: () => ({
    invoke: async (command: string) =>
      (command === 'get_settings' ? state.settings : null),
  }),
}))

vi.mock('../transport/mcp-wire', () => ({
  mcpListServers: async () => state.servers,
  mcpListExemptions: async () => [],
  mcpConnectAndReview: async () => { throw new Error('本测试不发起连接') },
  mcpResolveTrust: async () => { throw new Error('本测试不提交裁决') },
  mcpRevokeTrust: async () => {},
  mcpRevokeExemption: async () => {},
  tauriMcpSource: () => ({ listen: () => () => {} }),
}))

/** 词典直译（测试只关心键是否被用到，不关心八语种）。 */
function translate(key: McpTrustKey, params?: Record<string, unknown>): string {
  const template = (zh as Record<string, string>)[key] ?? key
  return template.replace(/\{(\w+)\}/g, (_match, name: string) => String(params?.[name] ?? `{${name}}`))
}

type ConnectorProps = Parameters<typeof McpConnector>[0]
const t = translate as unknown as ConnectorProps['t']

/** 后端在授权失败时返回的 reason 形态（原文来自 `sandbox::acl::grant_hint`）。 */
const BLOCKED_REASON =
  '启动 MCP server 失败（给「D:\\Programs\\nodejs」加沙箱 ACE 失败，已按备份还原：设置安全描述符失败：'
  + '拒绝访问 (os error 5)。原因：这个目录的权限当前用户改不了——它通常属于系统目录，或是由管理员权限'
  + '安装的软件（Windows 错误 5 = 拒绝访问）。可行的做法：把要运行的程序连同它依赖的解释器复制到你自己的'
  + '目录（例如工作区或用户目录），再把配置里的 command 指到新位置后重试）'

function blockedServer(): McpServerSummaryWire {
  return {
    id: 'e2e-echo',
    enabled: true,
    command: 'D:\\Programs\\nodejs\\node.exe',
    args: ['C:\\work\\server.mjs'],
    unsandboxed: false,
    allow_network: false,
    status: 'blocked',
    fingerprint: null,
    approved_tools: [],
    exemption_count: 0,
    reason: BLOCKED_REASON,
  }
}

beforeEach(() => {
  state.servers = []
  state.settings = { configReadOnly: null }
})

describe('MCP 连接器：blocked 的提示', () => {
  it('同时显示原因与可行做法，而不是只有「已阻止」', async () => {
    state.servers = [blockedServer()]
    render(<McpConnector t={t} />)

    expect(await screen.findByText('1 台 MCP server 被阻止启动')).toBeTruthy()
    // 原因：用户能看懂的那句 + 技术原因（错误 5）各出现一次。
    expect(screen.getByText(/这个目录的权限当前用户改不了/)).toBeTruthy()
    expect(screen.getByText(/错误 5/)).toBeTruthy()
    // 动作：指明下一步做什么。
    expect(screen.getByText(/把要运行的程序连同它依赖的解释器复制到你自己的目录/)).toBeTruthy()
    // 兜底：处理完怎么重试。
    expect(screen.getByText(/点「连接并审查」重试/)).toBeTruthy()
  })

  it('没有记录原因时不留空白，指向日志', async () => {
    state.servers = [{ ...blockedServer(), reason: null }]
    render(<McpConnector t={t} />)

    expect(await screen.findByText(/没有记录原因，请查看数据目录下 logs 里的日志/)).toBeTruthy()
  })
})

describe('MCP 连接器：配置只读', () => {
  it('配置解析失败时明说只读，并带上出错原因', async () => {
    state.servers = []
    state.settings = {
      configReadOnly:
        '解析配置失败: TOML parse error at line 12, column 1（文件未被改动：C:\\Users\\u\\AppData\\Roaming\\SHANIU\\config.toml）',
    }
    render(<McpConnector t={t} />)

    const banner = await screen.findByRole('alert')
    expect(banner.textContent).toContain('配置读取失败，已按只读运行')
    expect(banner.textContent).toContain('不会写回 config.toml')
    // 指向具体行，用户才改得动。
    expect(banner.textContent).toContain('line 12')
  })

  it('配置可写时不出现只读横幅', async () => {
    state.servers = []
    render(<McpConnector t={t} />)

    expect(await screen.findByText(zh['empty'])).toBeTruthy()
    expect(screen.queryByText(/配置读取失败/)).toBeNull()
  })
})

/** 通用设置页那一行用的是同一个组件，只需要按它自己的入参类型包一层。 */
const rowT = (key: 'configReadOnly', params: { reason: string }): string => translate(key, params)

describe('通用设置页：配置只读横幅', () => {
  it('与 MCP 页用同一段文案，且指向出错行', async () => {
    state.settings = { configReadOnly: '解析配置失败: TOML parse error at line 3, column 1' }
    render(<GeneralConfigReadOnlyRow t={rowT} />)

    const banner = await screen.findByRole('alert')
    expect(banner.textContent).toContain('配置读取失败，已按只读运行')
    expect(banner.textContent).toContain('line 3')
  })

  it('配置可写时这一行不渲染', async () => {
    state.settings = { configReadOnly: null }
    const { container } = render(<GeneralConfigReadOnlyRow t={rowT} />)
    await act(async () => { await Promise.resolve() })

    expect(container.textContent).toBe('')
  })
})
