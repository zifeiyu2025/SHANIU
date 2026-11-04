import { useEffect, useState } from 'react';
import { ChatView } from './components/chat/ChatView';
import { createSessionStore, type SessionStore } from './store/session-store';
import type { Transport } from './transport/types';

type LinkState = 'pending' | 'ok' | 'fail';

export function App({ transport }: { transport: Transport }) {
  const [store] = useState(() => createSessionStore());
  const [link, setLink] = useState<LinkState>('pending');

  useEffect(() => {
    let alive = true;
    transport
      .ping('m0')
      .then(
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
    <div className="flex h-screen flex-col bg-surface text-zinc-100">
      <header className="flex items-center justify-between border-b border-border-subtle px-4 py-2">
        <span className="font-semibold tracking-widest">SHANIU</span>
        <span
          data-testid="link-state"
          className={
            link === 'ok'
              ? 'text-xs text-emerald-400'
              : link === 'fail'
                ? 'text-xs text-red-400'
                : 'text-xs text-zinc-500'
          }
        >
          {link === 'ok' ? 'IPC 已连通' : link === 'fail' ? 'IPC 未连通' : '连接中…'}
        </span>
      </header>
      <div className="min-h-0 flex-1">
        <ChatView store={store} transport={transport} />
      </div>
    </div>
  );
}

export type { SessionStore };
