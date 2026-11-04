import { useEffect, useState } from 'react';
import { ChatView } from './components/chat/ChatView';
import { Sidebar } from './components/chrome/Sidebar';
import { TitleBar } from './components/chrome/TitleBar';
import { ToolPanel } from './components/chrome/ToolPanel';
import { createSessionStore, type SessionStore } from './store/session-store';
import type { Transport } from './transport/types';

type LinkState = 'pending' | 'ok' | 'fail';

export function App({ transport }: { transport: Transport }) {
  const [store] = useState(() => createSessionStore());
  const [link, setLink] = useState<LinkState>('pending');
  const [sidebarCollapsed, setSidebarCollapsed] = useState(false);
  const [rightOpen, setRightOpen] = useState(false);

  useEffect(() => {
    let alive = true;
    transport.ping('m0').then(
      () => alive && setLink('ok'),
      () => alive && setLink('fail'),
    );
    const off = transport.onEvent((msg) => store.dispatch(msg));
    return () => {
      alive = false;
      off();
    };
  }, [transport, store]);

  return (
    <div className="flex h-screen flex-col bg-surface text-text">
      <TitleBar
        collapsed={sidebarCollapsed}
        onToggleCollapse={() => setSidebarCollapsed((c) => !c)}
        rightOpen={rightOpen}
        onToggleRight={() => setRightOpen((o) => !o)}
      />
      <div className="flex min-h-0 flex-1">
        {!sidebarCollapsed && (
          <Sidebar store={store} onNewSession={() => store.reset()} />
        )}
        <main className="min-w-0 flex-1">
          <ChatView store={store} transport={transport} />
        </main>
        {rightOpen && <ToolPanel onClose={() => setRightOpen(false)} />}
      </div>
      {/* 连接状态以无侵入方式呈现（M0 验收观察点） */}
      <span data-testid="link-state" className="sr-only">
        {link === 'ok' ? 'IPC 已连通' : link === 'fail' ? 'IPC 未连通' : '连接中'}
      </span>
    </div>
  );
}

export type { SessionStore };
