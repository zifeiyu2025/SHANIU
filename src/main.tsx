import { bootShaniu } from './dsh-boot';
import { createBootProgress } from './host/boot-progress';
import { installWindowChrome } from './host/tauri-chrome';
import { installDesktopEnvironment } from './host/desktop-env';
import { installInputContextMenu } from './host/input-context-menu';
import { invoke } from '@tauri-apps/api/core';

// DSH 壳的基座样式（base.css：reset + boot page 变量）
import '@deepseek-ai/dsh-client-web/base.css';
// SHANIU 应用级版式微调（分栏控件隐藏等）
import './host/app.css';

const el = document.getElementById('root');
if (el === null) throw new Error('missing #root');

// L3 交接：启动页（index.html 的 #boot-splash）先提到 body 下。React 首次提交会清空
// 容器，留在 #root 里会被瞬间删掉，变成"启动页没了、应用还没画上"的硬切。
// 提到 body 后它保持覆盖，等应用首帧画好再由 createBootProgress 淡出（同底色，无闪）。
const splash = document.getElementById('boot-splash');
const status = splash?.querySelector<HTMLElement>('[data-boot-status]') ?? null;
if (splash !== null && status !== null) document.body.append(splash);
const boot = splash !== null && status !== null ? createBootProgress(splash, status) : null;

// 无边框窗口的标题按钮/拖拽/缩放手柄只属于 Tauri 宿主：纯浏览器联调下没有真窗口可控制。
// 装在 UI 挂载前——控制层只写 html 上的版式开关并挂自己的覆盖层，不读 DSH 渲染结果。
// 桌面运行时声明同样必须先于 bootShaniu：快捷键在服务构造时就按运行时选键位表。
if ('__TAURI_INTERNALS__' in window) {
  installWindowChrome();
  installDesktopEnvironment();
}

// 所有输入框的右键菜单：桌面发布版没有可用的 WebView2 默认菜单，统一由宿主自绘，
// 开发窗口与浏览器联调看到的是同一套行为。
installInputContextMenu();

// 主题缓存：index.html 的 L0 预涂读这里，决定启动页与应用底色用哪一套（静态
// index.html 拿不到宿主设置）。跟随每一次主题切换写回，而不是只在启动成功时写一次——
// 否则"切到浅色 → 重启"的第一次启动会先涂暗色再跳亮，正是要消除的那种闪。
const cacheResolvedTheme = (): void => {
  try {
    localStorage.setItem(
      'shaniu.theme.resolved',
      document.body.hasAttribute('data-ds-dark-theme') ? 'dark' : 'light',
    );
  } catch {
    // 存储不可用（隐私模式等）：下次启动退回跟随系统，不影响本次。
  }
};
cacheResolvedTheme();
new MutationObserver(cacheResolvedTheme).observe(document.body, {
  attributes: true,
  attributeFilter: ['data-ds-dark-theme'],
});

bootShaniu(el, { onStage: label => boot?.report(label) })
  .then(async () => {
    // 主包已接管：index.html 里的 12s 看门狗不再需要（它只兜"主包没跑起来"）。
    ;(window as Window & { __shaniuBootSettle?: () => void }).__shaniuBootSettle?.();
    // 窗口以 visible:false 创建（tauri.conf.json）。index.html 里的早期信号负责"尽早显示"，
    // 这里负责"必定显示"——即使那段内联脚本失效，界面已就绪时窗口也一定会出现。
    if ('__TAURI_INTERNALS__' in window) {
      void invoke('frontend_ready').catch(() => undefined);
    }
    await boot?.done();
  })
  .catch((reason: unknown) => {
    console.error(reason);
    ;(window as Window & { __shaniuBootSettle?: () => void }).__shaniuBootSettle?.();
    const detail = reason instanceof Error ? `${reason.message}\n${reason.stack ?? ''}` : String(reason);
    if (boot !== null) boot.fail({ message: detail });
    else el.textContent = `启动失败：${detail}`;
  });
