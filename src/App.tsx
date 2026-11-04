import { useEffect, useState } from 'react';
import { ChatView } from './components/chat/ChatView';
import { Sidebar, SidebarRail } from './components/chrome/Sidebar';
import { TitleBar } from './components/chrome/TitleBar';
import { ToolPanel } from './components/chrome/ToolPanel';
import { createSessionStore, type SessionStore } from './store/session-store';
import type { Transport } from './transport/types';

type LinkState = 'pending' | 'ok' | 'fail';

/**
 * 应用框架（复刻 dsh AppFrame）：40px 标题栏拖拽带（bg=侧栏填充），
 * 三列布局 auto 1fr auto；中列左上圆角 16px（AppFrame.module.css:27-47）。
 */
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
    <div className="flex h-screen flex-col overflow-hidden bg-sidebar text-primary">
      <TitleBar
        rightOpen={rightOpen}
        onToggleRight={() => setRightOpen((o) => !o)}
        link={link}
      />
      <div className="flex min-h-0 flex-1">
        {sidebarCollapsed ? (
          <SidebarRail onExpand={() => setSidebarCollapsed(false)} />
        ) : (
          <Sidebar store={store} onNewSession={() => store.reset()} onCollapse={() => setSidebarCollapsed(true)} />
        )}
        <main className="min-w-0 flex-1 overflow-hidden rounded-tl-[16px] bg-canvas">
          <ChatView store={store} transport={transport} />
        </main>
        {rightOpen && <ToolPanel onClose={() => setRightOpen(false)} />}
      </div>
    </div>
  );
}

export type { SessionStore };
