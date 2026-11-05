import { useCallback, useEffect, useState } from 'react';
import { ChatView } from './components/chat/ChatView';
import { EdgeResizeLayer } from './components/chrome/EdgeResizeLayer';
import { Sidebar, SidebarRail } from './components/chrome/Sidebar';
import { TitleBar } from './components/chrome/TitleBar';
import { ToolPanel } from './components/chrome/ToolPanel';
import { SettingsDialog } from './components/settings/SettingsDialog';
import { GearIcon } from './components/icons';
import { createSessionStore, type SessionStore } from './store/session-store';
import { workspaceDisplayName } from './utils/session-actions';
import type { Settings, Transport } from './transport/types';

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
  const [settingsOpen, setSettingsOpen] = useState(false);
  // header crumb 与侧栏工作区行的数据源：真取 settings，取不到就显示未设置，不做假文案
  const [settings, setSettings] = useState<Settings | null>(null);
  const reloadSettings = useCallback(() => {
    transport.getSettings().then(setSettings, () => setSettings(null));
  }, [transport]);

  useEffect(() => {
    let alive = true;
    transport.ping('m0').then(
      () => {
        if (!alive) return;
        setLink('ok');
        reloadSettings();
      },
      () => {
        if (alive) setLink('fail');
      },
    );
    const off = transport.onEvent((msg) => store.dispatch(msg));
    return () => {
      alive = false;
      off();
    };
  }, [transport, store, reloadSettings]);

  const workspaceLabel = workspaceDisplayName(settings?.workspaceRoot ?? '');
  const modelLabel = settings?.model?.trim() ?? '';

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
          <Sidebar
            onCollapse={() => setSidebarCollapsed(true)}
            store={store}
            transport={transport}
            workspaceLabel={workspaceLabel}
            onOpenSettings={() => setSettingsOpen(true)}
          />
        )}
        <main className="min-w-0 flex-1 overflow-hidden rounded-tl-[16px] bg-canvas">
          <ChatView
            store={store}
            transport={transport}
            workspaceLabel={workspaceLabel}
            modelLabel={modelLabel}
            sandboxMode={settings?.sandboxMode ?? 'read_only'}
            onOpenSettings={() => setSettingsOpen(true)}
            headerAction={
              <button
                type="button"
                aria-label="打开设置"
                title="设置"
                onClick={() => setSettingsOpen(true)}
                className="flex h-8 w-8 items-center justify-center rounded-sm text-tertiary transition-colors duration-100 hover:bg-hover hover:text-primary"
              >
                <GearIcon className="h-4 w-4" />
              </button>
            }
          />
        </main>
        {rightOpen && <ToolPanel onClose={() => setRightOpen(false)} />}
      </div>
      {settingsOpen && (
        <SettingsDialog
          transport={transport}
          onClose={() => {
            setSettingsOpen(false);
            // 关窗即重取：保存过就立刻反映到 crumb/侧栏，没保存也无害——多一次读而已
            reloadSettings();
          }}
        />
      )}
      <EdgeResizeLayer />
    </div>
  );
}

export type { SessionStore };
