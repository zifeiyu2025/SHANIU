import { bootShaniu } from './dsh-boot';

// DSH 壳的基座样式（base.css：reset + boot page 变量）
import '@deepseek-ai/dsh-client-web/base.css';

const el = document.getElementById('root');
if (el === null) throw new Error('missing #root');

bootShaniu(el).catch((reason: unknown) => {
  console.error(reason);
  el.textContent = `启动失败：${reason instanceof Error ? reason.message : String(reason)}`;
});
