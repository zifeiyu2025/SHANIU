import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import { App } from './App';
import { createMockTransport } from './transport/mock';
import { createTauriTransport } from './transport/tauri';
import type { Transport } from './transport/types';
import { initTheme } from './theme';
import './styles/global.css';

/** ADR-006：transport 注入——Tauri 环境走 IPC，纯浏览器开发走 mock。 */
function pickTransport(): Transport {
  return typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window
    ? createTauriTransport()
    : createMockTransport();
}

// 主题必须在首次渲染前落定：晚一步就会先闪一帧另一档配色（dsh 同样把 boot-theme 放在挂载前）
initTheme();

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <App transport={pickTransport()} />
  </StrictMode>,
);
