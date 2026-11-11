/**
 * SHANIU 的 DSH 静态组装入口（替换 DSH 的 manifest 扫描 + bundle loader）。
 * 组装范式照抄 DSH 官方测试样板 packages/client/ui-layout/tests/apply.client.spec.ts：
 * Context → SlotRegistry（随 renderer apply）→ locale → shortcuts → 各特性包 {inject, apply}。
 * 宿主侧服务（sessions/remote/configForms/remoteAccess）由 src/transport/ 适配层提供；
 * 尚未接线的能力一律经 stubs.ts 显式 stub，不做隐式降级。
 */
import { Context, Logger } from '@deepseek-ai/cordis'
import type { Exporter, Fiber, FiberState } from '@deepseek-ai/cordis'
import { apply as rendererApply } from '@deepseek-ai/dsh-client-ui-renderer/client'
import { apply as themeApply, inject as themeInject } from '@deepseek-ai/dsh-client-ui-theme/client'
import { apply as layoutApply, inject as layoutInject } from '@deepseek-ai/dsh-client-ui-layout/client'
import { apply as uiSessionApply, inject as uiSessionInject } from '@deepseek-ai/dsh-client-ui-session/client'
import { apply as uiWorkspaceApply, inject as uiWorkspaceInject } from '@deepseek-ai/dsh-client-ui-workspace/client'
import { apply as directoryPickerBrowseApply, inject as directoryPickerBrowseInject } from '@deepseek-ai/dsh-client-ui-directory-picker-browse/client'
import { apply as directoryPickerNativeApply, inject as directoryPickerNativeInject } from '@deepseek-ai/dsh-client-ui-directory-picker-native/client'
import { apply as uiSidebarApply, inject as uiSidebarInject } from '@deepseek-ai/dsh-client-ui-sidebar/client'
import { apply as uiSidebarRightApply, inject as uiSidebarRightInject } from '@deepseek-ai/dsh-client-ui-sidebar-right/client'
import { apply as uiSidebarFilesApply, inject as uiSidebarFilesInject } from '@deepseek-ai/dsh-client-ui-sidebar-files/client'
import { apply as uiSidebarBrowserApply, inject as uiSidebarBrowserInject } from '@deepseek-ai/dsh-client-ui-sidebar-browser/client'
import { apply as uiSidebarTerminalApply, inject as uiSidebarTerminalInject } from '@deepseek-ai/dsh-client-ui-sidebar-terminal/client'
import { apply as uiConversationApply, inject as uiConversationInject } from '@deepseek-ai/dsh-client-ui-conversation/client'
import { apply as uiChatApply, inject as uiChatInject } from '@deepseek-ai/dsh-client-ui-chat/client'
import { apply as uiSubagentApply, inject as uiSubagentInject } from '@deepseek-ai/dsh-client-ui-subagent/client'
import { apply as uiSecretMaskApply, inject as uiSecretMaskInject } from '@deepseek-ai/dsh-client-ui-secret-mask/client'
import { apply as uiWorkflowRunApply, inject as uiWorkflowRunInject } from '@deepseek-ai/dsh-client-ui-workflow-run/client'
import { apply as uiSidebarDocumentPreviewApply, inject as uiSidebarDocumentPreviewInject } from '@deepseek-ai/dsh-client-ui-sidebar-documentpreview/client'
import { apply as uiApprovalApply, inject as uiApprovalInject } from '@deepseek-ai/dsh-client-ui-approval/client'
import { apply as uiToolApply, inject as uiToolInject } from '@deepseek-ai/dsh-client-ui-tool/client'
import { apply as uiTrajectoryApply, inject as uiTrajectoryInject } from '@deepseek-ai/dsh-client-ui-trajectory/client'
import { apply as uiUserQuestionsApply, inject as uiUserQuestionsInject } from '@deepseek-ai/dsh-client-ui-user-questions/client'
import { apply as uiModelSelectionApply, inject as uiModelSelectionInject } from '@deepseek-ai/dsh-client-ui-model-selection/client'
import { apply as uiPlanApply, inject as uiPlanInject } from '@deepseek-ai/dsh-client-ui-plan/client'
import { apply as uiGoalApply, inject as uiGoalInject } from '@deepseek-ai/dsh-client-ui-goal/client'
import { apply as jobControllerApply, inject as jobControllerInject } from '@deepseek-ai/dsh-api-job-controller/client'
import { apply as uiJobsApply, inject as uiJobsInject } from '@deepseek-ai/dsh-client-ui-jobs/client'
import { apply as uiPermissionApply, inject as uiPermissionInject } from '@deepseek-ai/dsh-client-ui-permission-presets/client'
import { apply as uiDeliverablesApply, inject as uiDeliverablesInject } from '@deepseek-ai/dsh-client-ui-deliverables/client'
import { apply as uiAttachmentApply, inject as uiAttachmentInject } from '@deepseek-ai/dsh-client-ui-attachment/client'
import { apply as uiPluginManagerApply, inject as uiPluginManagerInject } from '@deepseek-ai/dsh-client-ui-plugin-manager/client'
import { apply as uiScheduleApply, inject as uiScheduleInject } from '@deepseek-ai/dsh-client-ui-schedule/client'
import { apply as uiContextMenuApply, inject as uiContextMenuInject } from '@deepseek-ai/dsh-client-ui-context-menu/client'
import { apply as uiFileDragApply, inject as uiFileDragInject } from '@deepseek-ai/dsh-client-ui-file-drag/client'
import { apply as uiFileActionsApply, inject as uiFileActionsInject } from '@deepseek-ai/dsh-client-ui-file-actions/client'
import { apply as uiOpenInAppApply, inject as uiOpenInAppInject } from '@deepseek-ai/dsh-client-ui-open-in-app/client'
import { apply as uiReferenceApply, inject as uiReferenceInject } from '@deepseek-ai/dsh-client-ui-reference/client'
import { apply as uiSkillApply, inject as uiSkillInject } from '@deepseek-ai/dsh-client-ui-skill/client'
import { installDesktopFileActions } from './file-actions/desktop-bridge'
import { installAttachmentUploadFetch } from './transport/attachment-upload'
import { installOpenInAppRoutes } from './open-in-app/route-fetch'
import { installExternalLinkOpener } from './external-link/route'
import { apply as uiInputTriggerApply, inject as uiInputTriggerInject } from '@deepseek-ai/dsh-client-ui-input-trigger/client'
import { apply as uiCommandsApply, inject as uiCommandsInject } from '@deepseek-ai/dsh-client-ui-commands/client'
import { apply as uiEntryCustomizationApply, inject as uiEntryCustomizationInject } from '@deepseek-ai/dsh-client-ui-entry-customization/client'
import { apply as fileUploadApply, inject as fileUploadInject } from '@deepseek-ai/dsh-client-file-upload/client'
import { apply as resourcesApply, inject as resourcesInject } from '@deepseek-ai/dsh-client-resources/client'
import { apply as workspaceFilesProviderApply, inject as workspaceFilesProviderInject } from '@deepseek-ai/dsh-api-workspace-files/client'
import { apply as workspaceControllerApply, inject as workspaceControllerInject } from '@deepseek-ai/dsh-api-workspace-controller/client'
import { apply as sessionControllerApply, inject as sessionControllerInject } from '@deepseek-ai/dsh-api-session-controller/client'
import { apply as uiSettingsApply, inject as uiSettingsInject } from '@deepseek-ai/dsh-client-ui-settings/client'
import { apply as uiSettingsGeneralApply, inject as uiSettingsGeneralInject } from '@deepseek-ai/dsh-client-ui-settings-general/client'
import { apply as uiSettingsModelsApply, inject as uiSettingsModelsInject } from '@deepseek-ai/dsh-client-ui-settings-models/client'
import { ONBOARDING_CONFIG_GLOBAL } from '@deepseek-ai/dsh-client-ui-settings-models/onboarding-config'
import ShortcutsService from '@deepseek-ai/dsh-client-shortcuts/client'
import { apply as localeApply, inject as localeInject } from '@deepseek-ai/dsh-client-locale/client'
import {
  StubRemoteAccess, installRemoteNamespaceStubs,
  stubRemote, stubSessions,
} from './transport/stubs'
import { createAdapter } from './transport/adapter'
import { hostServices } from './transport/host-services'
import { tauriBridge } from './transport/tauri'
import { createWsBridge } from './transport/ws-bridge'
import {
  installMockRemoteNamespaces, mockConnection, mockModules, mockRemote, mockTypert,
  setDefaultWorkspacePath,
} from './transport/mock'
import { apply as brandApply, inject as brandInject } from './brand'
import { apply as headerActionsApply, inject as headerActionsInject } from './header-actions'
import { apply as mcpTrustApply, inject as mcpTrustInject } from './mcp-trust'
import { apply as modelConfigApply, inject as modelConfigInject } from './model-config'
import { apply as modelCapabilitiesApply, inject as modelCapabilitiesInject } from './model-capabilities'
// 默认目录说明（A0-6）：通用设置页一行只读信息，不改工作区选择逻辑。
import { apply as defaultDirApply, inject as defaultDirInject } from './default-dir'
// 附件占用（A0-7）：通用设置页一行只读统计，只让量级可见，不提供清理。
import { apply as attachmentUsageApply, inject as attachmentUsageInject } from './attachment-usage'
import { apply as sandboxStatusApply, inject as sandboxStatusInject } from './sandbox-status'
import { apply as userTerminalSourceApply, inject as userTerminalSourceInject } from './terminal-source'
import { apply as trayHintApply, inject as trayHintInject } from './tray-hint'
import { apply as quitGuardApply, inject as quitGuardInject } from './quit-guard'

export interface BootHandle {
  readonly ctx: Context
  dispose(): Promise<void>
}

/** 按依赖序安装一个 {inject, apply} 特性插件。 */
type FeaturePlugin = { inject: readonly string[]; apply: (ctx: Context) => unknown }

/** 纯浏览器开发态走 mock 数据源（SH-008 mock 联调）；Tauri 内默认走显式 stub 等待接线。 */
function usingTauriBackend(): boolean {
  return typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window
}

/**
 * 桌面壳的默认工作区以宿主解析结果为准（`<数据根>\Workspace`，docs/01 §9.3）；
 * 浏览器联调没有宿主，保留 mock 的展示路径。返回的家目录给 connection 基线用
 * （界面缩写显示路径）。读取失败只告警，不拦启动——那时家目录取空串，
 * 界面按完整路径显示，比因为读不到目录而起不来好。
 */
async function applyProductWorkspacePath(): Promise<{ homeDir: string }> {
  try {
    const { invoke } = await import('@tauri-apps/api/core')
    const dirs = await invoke<{ workspaceRoot?: string; homeDir?: string }>('get_product_dirs')
    if (typeof dirs.workspaceRoot === 'string' && dirs.workspaceRoot !== '') {
      setDefaultWorkspacePath(dirs.workspaceRoot)
    }
    return { homeDir: typeof dirs.homeDir === 'string' ? dirs.homeDir : '' }
  } catch (reason) {
    console.warn('[SHANIU] 读取产品目录失败，默认工作区退回 mock 展示路径', reason)
    return { homeDir: '' }
  }
}

/**
 * 壳内数据源开关（开发期）：true = 桌面窗口也装 mock 事实源，用来看界面的实际效果；
 * false = 桌面窗口装适配层（`transport/adapter.ts` + `transport/settings-face.ts`），
 * 事件流与设置面都走宿主命令；仍未接线的命令面继续落到显式 stub 核销清单。
 * 这是有意偏离 ADR-006 的临时口径——docs/01 风险表把"适配层掩盖能力缺口"列为风险，
 * 所以未接线项（SH-025 审批 UI、SH-033 历史回放…）核销前，浏览器开发态仍留在 mock 上，
 * 只有真实桌面切过去。SH-094 批 3 已把桌面事件面切到 loopback WS（`ws-bridge.ts`），
 * gateway 不可用时由它退回 Tauri 广播。
 */
const MOCK_IN_DESKTOP = false

/** cordis `FiberState` 序号：const enum 在 isolatedModules 下不能按值导入，故取序号并锁定类型。 */
const FIBER_STATE_ACTIVE: FiberState = 2
const FIBER_STATE_FAILED: FiberState = 3
const FIBER_STATE_DISPOSED: FiberState = 4

/** fiber → 可读标签（cordis 会把 `apply` 回调名丢掉，只剩 root，所以用 boot 记下的标签）。 */
function fiberLabel(labels: ReadonlyMap<number, string>, fiber: Fiber): string {
  return (fiber.uid === null ? undefined : labels.get(fiber.uid)) ?? `${fiber.name}#${String(fiber.uid)}`
}

/** FAILED fiber 的失败原因：cordis 没有公开 getter，只存在私有字段里（fiber.ts:208）。 */
function fiberError(fiber: Fiber): string {
  const error = (fiber as unknown as { _error?: unknown })._error
  return error instanceof Error ? error.message : String(error)
}

/**
 * 插件装载失败上报：订阅 `internal/status` 事件，不再用 boot 收尾快照。
 *
 * 为什么不是快照：`apply` 里 `await` 之后才抛的异常，落成 FAILED 的时机晚于任何
 * 「等两个宏任务再扫一遍」；boot 之后才失败的也永远扫不到。cordis 每次状态变更都会
 * `emit('internal/status')`（fiber.ts:592），而 `_reload` 的 catch 先写 `_error` 再更新状态
 * （fiber.ts:693-708），所以订阅事件能拿到全部失败，且不需要猜等待时长。
 *
 * @param isDisposing - 拆卸窗口抑制。vendored cordis 的 `_unload` 自己 catch 了 disposer 抛错，
 *   只写日志、不置 FAILED（fiber.ts:720-730）；但拆卸期间仍在跑的 reload 若 apply 抛错则会置
 *   FAILED——那不是装载失败。把退出期的这类失败报成「插件装载失败」会训练人忽略这条线，
 *   假红比没红更坏（AGENTS §4.2）。
 */
export function watchPluginFailures(
  ctx: Context,
  labels: ReadonlyMap<number, string>,
  isDisposing: () => boolean,
): void {
  ctx.on('internal/status', (fiber) => {
    if (fiber.state !== FIBER_STATE_FAILED) return
    if (isDisposing()) return
    ctx.logger.error('[SHANIU] 插件装载失败 · %s：%s', fiberLabel(labels, fiber), fiberError(fiber))
  })
}

/**
 * 开发态排查辅助：打印一次 boot 完成时仍未 ACTIVE 的插件，附上缺的 inject 服务名。
 *
 * 这不是错误上报——PENDING 是合法的等待态（插件在等依赖）。这里只给排查的人一张
 * 清单，措辞不叫失败；生产分支不调用（调用点由 `import.meta.env.DEV` 静态裁剪）。
 */
export function dumpPendingPlugins(ctx: Context, labels: ReadonlyMap<number, string>): void {
  const rows: string[] = []
  for (const runtime of ctx.registry.values()) {
    for (const fiber of runtime.fibers) {
      if (fiber.state === FIBER_STATE_ACTIVE || fiber.state === FIBER_STATE_DISPOSED) continue
      // 只比「声明要注入的名字」与「此刻注册表里真有的服务」；optional 注入不参与，
      // 它们不阻塞 ACTIVE，不会有对应 fiber。
      const missing = Object.keys(fiber.inject ?? {})
        .filter(name => ctx.reflect.get(name, false) === undefined)
      rows.push(
        `${fiberLabel(labels, fiber)} state=${fiber.state}` +
        (missing.length === 0 ? '' : ` 缺注入=[${missing.join(', ')}]`),
      )
    }
  }
  if (rows.length === 0) return
  console.info('[SHANIU][dev] boot 完成时仍未 ACTIVE 的插件（排查用，不代表失败）:\n%s', rows.join('\n'))
}

/** 启动阶段回调：只上报真实存在的阶段（启动页据此显示，见 src/host/boot-progress.ts）。 */
export interface BootOptions {
  onStage?: (label: string) => void
  /**
   * 是否挂载 UI（默认 true）。置 false 只装配服务与槽位、不渲染，
   * 供 boot 级装载健康测试使用：jsdom 没有真实布局，挂载会在测量代码里崩。
   */
  mount?: boolean
}

/** 组装并挂载完整 UI；失败时把原因抛给调用方（入口页自绘错误）。 */
export async function bootShaniu(container: HTMLElement, options: BootOptions = {}): Promise<BootHandle> {
  const root = new Context()
  // cordis 默认 exporter 只把日志写进环形缓冲（logger.ts:213），宿主不接 console 出口时
  // 连 error 都没有声音——插件装载失败因此完全不可见。这里只放行 error（level 0）。
  const consoleExporter: Exporter = {
    levels: { default: 0 },
    export: (message) => { console.error(Logger.format(consoleExporter, message)) },
  }
  root.logger.exporter(consoleExporter)
  const mockData = !usingTauriBackend() || MOCK_IN_DESKTOP
  const realData = usingTauriBackend() && !MOCK_IN_DESKTOP
  /** 桌面适配层：卸载会话时要一起拆掉事件订阅。 */
  let adapter: { remote: unknown; dispose(): void } | undefined
  /**
   * 全局补丁的还原器（仅桌面态）。这些 install* 直接改 `globalThis`，
   * 不属于 cordis 上下文，`fiber.dispose()` 不会碰它们——必须由 dispose 显式逆序撤回。
   */
  const restoreGlobals: (() => void)[] = []
  /** 装载失败上报用的标签表：feature 装载时按 uid 登记（cordis 不保留 apply 名）。 */
  const bootLabels = new Map<number, string>()
  /** 拆卸窗口标志：抑制「退出期仍在跑的 reload 失败」被记成插件装载失败（见 watchPluginFailures）。 */
  let disposing = false
  // 必须在装载任何插件之前订阅：晚于第一个 fiber 就会漏掉它的装载期失败。
  watchPluginFailures(root, bootLabels, () => disposing)

  // 模型设置页的公开引导开关：SHANIU 没有 DeepSeek 云端账号，
  // 关掉自动弹出的官方登录引导（该 payload 就是上游为此留的接入口）。
  ;(globalThis as Partial<Record<typeof ONBOARDING_CONFIG_GLOBAL, unknown>>)[ONBOARDING_CONFIG_GLOBAL] = {
    credentialOnboarding: false,
  }

  // 渲染基座：SlotRegistry + slot renderer + ctx.uiRenderer.mount
  await root.plugin({ inject: [], apply: rendererApply }).await()

  // 宿主侧服务：适配层（桌面真实后端）、mock（浏览器联调）或 stub（等待接线的核销清单）
  if (realData) {
    // 原始文件动作的桌面适配面（SH-092 批 2）：上游 ui-file-actions 从这里取
    // `dshDesktop.fileActions`；保存目标只由命令内部的原生对话框产生。
    // 三个 install* 都改 globalThis，返回值是还原器——全部收好，dispose 逆序撤回。
    restoreGlobals.push(
      installDesktopFileActions(tauriBridge()),
      // open-in-app 的三条 Host HTTP 路由（SH-092 批 3）接到 Tauri 命令；vendored
      // ui-open-in-app 只经全局 fetch 访问相对路径，源码无需改动。
      installOpenInAppRoutes(tauriBridge()),
      // 外链（SH-137）：上游三处 `window.open` 入口只认全局函数，这里换成
      // `open_external_url`——WebView 里原生 open 会把整个应用导航到外网。
      installExternalLinkOpener(tauriBridge()),
      // 附件上传（SH-116 批 1）：file-upload 的 fetch 钩子接到 `upload_attachment`。
      // 必须在 fileUpload 插件构造前注入（本分支早于 features 挂载）。
      installAttachmentUploadFetch(),
    )
    // 默认工作区基线必须在挂载前写入；家目录给 connection 基线缩写路径用
    options.onStage?.('准备工作区')
    const { homeDir } = await applyProductWorkspacePath()
    adapter = createAdapter({
      // SH-094 批 3：命令面仍走 Tauri invoke，事件面切 loopback WS gateway；
      // gateway 连续不可用时桥自己退回 Tauri 全局广播（只退一次，不双通道投递）。
      bridge: createWsBridge(tauriBridge()),
      // 审批卡靠监听器的 `this` 认会话，而会话作用域 Context 由 sessions 服务持有
      // （装配时还没装好），所以给的是"派发时再取"的闭包。
      approvalScope: (sessionId: string): unknown => {
        const sessions = root.get('sessions') as unknown
        const scoped = sessions as { scope?: (id: string) => unknown } | undefined
        return typeof scoped?.scope === 'function' ? scoped.scope(sessionId) : undefined
      },
    })
    const covered = ['session', 'workspace', 'workspaceFiles', 'fileReferences', 'sessionReferenceResolver', 'skills', 'goals', 'job', 'toolResults', 'settings', 'llm', 'credentials', 'commands', 'permissionPresets', 'subagents', 'pluginInventory', 'pluginManager', 'pluginRegistryProbe', 'schedule']
    root.provide('remote', adapter.remote)
    // 适配层已接管的名字不再被 stub 覆盖：后注册的 stub 会让"消息流能跑、
    // 设置页永远抛错"，而抛错看起来像后端坏了，不像接线接反了。
    installRemoteNamespaceStubs(root, covered)
    for (const ns of covered) {
      root.reflect.provide(`remote.${ns}`, (adapter.remote as Record<string, unknown>)[ns])
    }
    const services = hostServices(homeDir)
    root.provide('connection', services.connection)
    root.provide('typert', services.typert)
    root.provide('modules', services.modules)
  } else if (!mockData) {
    root.provide('remote', stubRemote())
    installRemoteNamespaceStubs(root)
    root.provide('sessions', stubSessions())
    root.provide('modules', mockModules())
  } else {
    // 默认工作区基线：桌面壳用宿主解析出的 <数据根>\Workspace（必须在挂载前写入）
    if (usingTauriBackend()) await applyProductWorkspacePath()
    root.provide('remote', mockRemote())
    installMockRemoteNamespaces(root)
    root.provide('connection', mockConnection())
    root.provide('typert', mockTypert())
    root.provide('modules', mockModules())
    // 真实 sessions 服务（ClientSessions 运行时）吃 mock 的 remote.session 面：
    // list 拉预置行、control/follow 流空基线（会话内容原样渲染为空态）
  }
  // Service 子类：构造即自注册 'remoteAccess' 并发布 useRemoteAccess 全局座席。
  // mock 态与桌面适配层态都授权：本地单用户壳没有远端 ACL 可咨询，命令面就在
  // 进程内执行；工具要不要执行仍由 permission broker 裁决（SH-025），不受这里影响。
  // 纯 stub 态全拒（那条路径下任何命令面都会抛错，授权只会让 UI 显得在 work）。
  new StubRemoteAccess(root, mockData || realData)

  // 基础服务：locale 插件（词典注册 + slots.installLocale，渲染机合成 t 座席）
  // → shortcuts（依赖 locale）→ theme（依赖 slots/locale/remote/configForms）
  await root.plugin({ inject: [...localeInject], apply: localeApply }).await()
  await root.plugin(ShortcutsService).await()
  await root.plugin({ inject: [...themeInject], apply: themeApply }).await()

  // session 作用域适配器：AppFrame 的 'session-maybe' 出口渲染必需
  await root.plugin({ inject: [...uiSessionInject], apply: uiSessionApply }).await()

  // batch-2 特性包（按依赖序；每个包的 inject 见其 apply 模块）：
  // 资源/上传/拖拽/右键菜单/输入触发/命令 → 工作区 → 会话主区 → 右栏/侧栏
  const features: FeaturePlugin[] = [
    // ui-settings 必须最先：提供 configForms（本地 schema 表单，不依赖后端），theme 依赖它
    { inject: uiSettingsInject, apply: uiSettingsApply },
    { inject: resourcesInject, apply: resourcesApply },
    // 文件资源 provider（SH-092）：文档预览的 `file` 资源解析靠它，缺了预览只会说
    // 「文件资源服务不可用」。数据面是上面的 `remote.workspaceFiles`。
    { inject: workspaceFilesProviderInject, apply: workspaceFilesProviderApply },
    { inject: fileUploadInject, apply: fileUploadApply },
    { inject: sessionControllerInject, apply: sessionControllerApply },
    { inject: uiFileDragInject, apply: uiFileDragApply },
    // 原始文件动作（SH-092 批 2）：下载/另存为/复制图片。桌面适配面是
    // 原生保存对话框（路径只由用户手势产生），浏览器态退回 anchor 下载。
    { inject: uiFileActionsInject, apply: uiFileActionsApply },
    // 打开方式/在文件管理器中显示/复制路径（SH-092 批 3）。
    { inject: uiOpenInAppInject, apply: uiOpenInAppApply },
    // @ 文件/会话引用（SH-090 批 1：文件候选接真，会话候选如实空表）。
    { inject: uiReferenceInject, apply: uiReferenceApply },
    // / 技能触发 + skill 工具行（SH-091）。
    { inject: uiSkillInject, apply: uiSkillApply },
    { inject: uiContextMenuInject, apply: uiContextMenuApply },
    { inject: uiInputTriggerInject, apply: uiInputTriggerApply },
    { inject: uiCommandsInject, apply: uiCommandsApply },
    { inject: workspaceControllerInject, apply: workspaceControllerApply },
    { inject: uiWorkspaceInject, apply: uiWorkspaceApply },
    // 会话 View / 左栏 / 右栏 More 菜单（SH-093，上游 ui-entry-customization 源码不改）。
    // `创建你的插件` 用的是 Creator 预设的草稿；SHANIU 没有 agent preset，`agentPreset`
    // 会被适配层忽略，只保留"新建空白会话 + 待审阅草稿"这一半语义（已在 05 声明）。
    { inject: uiEntryCustomizationInject, apply: uiEntryCustomizationApply },
    // 目录选择：桌面壳弹系统「选择文件夹」对话框；纯浏览器没有系统对话框，退回应用内浏览
    usingTauriBackend()
      ? { inject: directoryPickerNativeInject, apply: directoryPickerNativeApply }
      : { inject: directoryPickerBrowseInject, apply: directoryPickerBrowseApply },
    { inject: uiConversationInject, apply: uiConversationApply },
    { inject: uiSidebarRightInject, apply: uiSidebarRightApply },
    // SH-071 用户终端：先装数据源（提供 `webTerminals`，走宿主的裸进程 ConPTY），
    // 再装上游 UI 包。只在桌面态挂载——浏览器联调没有本地进程可连，挂了只会是假入口。
    // 托盘提示同属终端通道：关窗口只是隐藏到托盘，仍有会话在跑时宿主发一次事件。
    ...(realData
      ? [{ inject: userTerminalSourceInject, apply: userTerminalSourceApply } as FeaturePlugin,
        { inject: uiSidebarTerminalInject, apply: uiSidebarTerminalApply } as FeaturePlugin,
        { inject: trayHintInject, apply: trayHintApply } as FeaturePlugin,
        // 真退出前先盘点在跑的任务：没有就直接退，有就给三个选项。
        { inject: quitGuardInject, apply: quitGuardApply } as FeaturePlugin]
      : []),
    { inject: uiSidebarFilesInject, apply: uiSidebarFilesApply },
    { inject: uiSidebarBrowserInject, apply: uiSidebarBrowserApply },
    { inject: uiSidebarInject, apply: uiSidebarApply },
    { inject: brandInject, apply: brandApply },
    { inject: headerActionsInject, apply: headerActionsApply },
    { inject: modelConfigInject, apply: modelConfigApply },
    { inject: uiSettingsGeneralInject, apply: uiSettingsGeneralApply },
    { inject: uiSettingsModelsInject, apply: uiSettingsModelsApply },
    // 能力视图（SH-132）：走设置页服务商卡片的扩展点，不改 vendored 模型行。
    { inject: modelCapabilitiesInject, apply: modelCapabilitiesApply },
    // 默认目录说明：`data\Workspace` 只在实际使用时创建，界面必须与这条行为一致。
    { inject: defaultDirInject, apply: defaultDirApply },
    // 附件占用：只报数，不当清理入口（清理是破坏性操作，先让量级可见）。
    { inject: attachmentUsageInject, apply: attachmentUsageApply },
    // MCP 信任页（SH-063）：首连指纹审查 / 逐工具风险 / 撤销信任与豁免。
    { inject: mcpTrustInject, apply: mcpTrustApply },
    // 沙箱状态条（SH-021）：常驻展示权限档 / 强制完整度 / OS 机制（网络不参与档位，不展示）。
    { inject: sandboxStatusInject, apply: sandboxStatusApply },
    { inject: uiChatInject, apply: uiChatApply },
    // 子代理泳道 UI（SH-109）：会话头目录树/面包屑切换/侧栏子会话聊天。
    // 数据面由 transport 提供 subagentCatalog/subagentTiming 投影帧。
    { inject: uiSubagentInject, apply: uiSubagentApply },
    // 对话流补全：用户消息里的密钥打码、工作流节点、文档预览页签
    // （赞/踩反馈不挂：本地客户端没有反馈出口，点了只写内存属于假动作）
    { inject: uiSecretMaskInject, apply: uiSecretMaskApply },
    { inject: uiWorkflowRunInject, apply: uiWorkflowRunApply },
    { inject: uiSidebarDocumentPreviewInject, apply: uiSidebarDocumentPreviewApply },
    { inject: uiApprovalInject, apply: uiApprovalApply },
    { inject: uiToolInject, apply: uiToolApply },
    { inject: uiTrajectoryInject, apply: uiTrajectoryApply },
    { inject: uiUserQuestionsInject, apply: uiUserQuestionsApply },
    { inject: uiModelSelectionInject, apply: uiModelSelectionApply },
    { inject: uiPlanInject, apply: uiPlanApply },
    // 目标条（SH-088）：输入卡 dock 的 GoalBar + `/goal` 用户文本引用；
    // 数据面是 transport 的 remote.goals 与 goal 投影/activation 事件帧。
    { inject: uiGoalInject, apply: uiGoalApply },
    // 后台作业面（SH-089）：job-controller 的 `ctx.jobs` 服务 + 会话头作业指示。
    // 只有适配层提供 remote.job 时才挂；mock 态没有作业事实源，挂上会一直等注入。
    ...(realData
      ? [{ inject: jobControllerInject, apply: jobControllerApply } as FeaturePlugin,
        { inject: uiJobsInject, apply: uiJobsApply } as FeaturePlugin]
      : []),
    { inject: uiPermissionInject, apply: uiPermissionApply },
    { inject: uiDeliverablesInject, apply: uiDeliverablesApply },
    { inject: uiAttachmentInject, apply: uiAttachmentApply },
    // 侧栏面板：插件管理（remote 面只读不可管理）与自动化任务（空目录）
    { inject: uiPluginManagerInject, apply: uiPluginManagerApply },
    { inject: uiScheduleInject, apply: uiScheduleApply },
  ]
  /** 装载失败项按 fiber uid 关联到这里；上报本身由 boot 早期订阅的事件完成。 */
  for (const [index, feature] of features.entries()) {
    // 真实进度：已装载数量 / 总数（不编造比例，见 boot-progress.ts 的约束）
    options.onStage?.(`装载界面插件 ${index + 1}/${features.length}`)
    const fiber = root.plugin({ inject: [...feature.inject], apply: feature.apply })
    if (fiber.uid !== null) {
      bootLabels.set(fiber.uid, `features[${index + 1}] inject=[${feature.inject.join(', ')}]`)
    }
    await fiber.await()
  }

  // 布局壳：占据 root slot（sidebar/main/rightbar/shell.*）
  await root.plugin({ inject: [...layoutInject], apply: layoutApply }).await()
  await new Promise<void>(resolve => setTimeout(resolve, 0))

  // 挂载：等价 DSH mountClient —— 等 uiRenderer 就位后 mount

  // 首帧后再跑一轮微任务：部分 Service 经 ctx.plugin(Class, config) 异步实例化（fiber 微任务），
  // 同步挂载会在首次渲染时读到未就位的服务（conversation 等）并让槽位边界卡死。
  // 注意这里不再扫描装载失败——失败由 watchPluginFailures 在发生的那一刻上报，
  // 快照只能看到此刻已经 FAILED 的，await 之后才抛的会漏。
  await new Promise<void>(resolve => setTimeout(resolve, 0))
  await Promise.resolve()

  const uiRenderer = root.get('uiRenderer')
  if (uiRenderer === undefined) throw new Error('dsh-boot: uiRenderer 服务未就位')
  options.onStage?.('渲染界面')
  const unmount = options.mount === false ? () => {} : uiRenderer.mount(container)
  // 开发期诊断：真机排查槽位/视图注册（例如"会话头 More 不出现"）时读根 Context。
  let devDumpTimer: ReturnType<typeof setTimeout> | undefined
  if (import.meta.env.DEV) {
    ;(globalThis as Record<string, unknown>).__SHANIU_BOOT_CTX__ = root
    // 固定时刻再列一次仍未 ACTIVE 的插件：此刻还没就位的多是在等依赖或装载较慢，
    // 清单只做排查参考，不判定失败。生产分支不执行（上面的 DEV 静态裁剪）。
    devDumpTimer = setTimeout(() => { dumpPendingPlugins(root, bootLabels) }, 3000)
  }

  return {
    ctx: root,
    async dispose() {
      // 先立标志再拆：disposer 抛错会被 cordis 记成 FAILED，那不是装载失败。
      disposing = true
      if (devDumpTimer !== undefined) clearTimeout(devDumpTimer)
      unmount()
      adapter?.dispose()
      // 逆序还原全局补丁：后装的先撤，与安装顺序严格对称。不还原的代价：
      // `globalThis.fetch` 被永久包裹，同一进程里重复 boot 会层层套娃，
      // dispose 之后再 boot 的路径全部穿过上一次的闭包。
      // splice(0) 同时清空，重复 dispose 不会二次调用还原器。
      for (const restore of restoreGlobals.splice(0).reverse()) restore()
      await root.fiber.dispose()
    },
  }
}
