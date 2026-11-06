import { bootShaniu } from './dsh-boot';
import { installWindowChrome } from './host/tauri-chrome';
import { installDesktopEnvironment } from './host/desktop-env';

// DSH 壳的基座样式（base.css：reset + boot page 变量）
import '@deepseek-ai/dsh-client-web/base.css';
// SHANIU 应用级版式微调（分栏控件隐藏等）
import './host/app.css';

const el = document.getElementById('root');
if (el === null) throw new Error('missing #root');

// 无边框窗口的标题按钮/拖拽/缩放手柄只属于 Tauri 宿主：纯浏览器联调下没有真窗口可控制。
// 装在 UI 挂载前——控制层只写 html 上的版式开关并挂自己的覆盖层，不读 DSH 渲染结果。
// 桌面运行时声明同样必须先于 bootShaniu：快捷键在服务构造时就按运行时选键位表。
if ('__TAURI_INTERNALS__' in window) {
  installWindowChrome();
  installDesktopEnvironment();
}

bootShaniu(el).catch((reason: unknown) => {
  console.error(reason);
  const detail = reason instanceof Error ? `${reason.message}\n${reason.stack ?? ''}` : String(reason);
  el.textContent = `启动失败：${detail}`;
});
