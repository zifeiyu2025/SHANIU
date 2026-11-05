import { FolderIcon, GearIcon, PanelLeftIcon, PlusIcon } from '../icons';
import { startNewSession } from '../../utils/session-actions';
import type { SessionStore } from '../../store/session-store';
import type { Transport } from '../../transport/types';

/**
 * 左侧栏（280px，bg=侧栏填充）。
 * 只放**当前真有数据/真行为**的入口：新建会话、工作区、设置。
 * 会话列表要等 rollout 持久化（SH-030/031/033）才有数据源，画假列表等于给 UI 埋一条
 * "看起来能用、点了没反应"的债，宁缺。
 */
export function Sidebar({
  onCollapse,
  store,
  transport,
  workspaceLabel,
  onOpenSettings,
}: {
  onCollapse: () => void;
  store: SessionStore;
  transport: Transport;
  workspaceLabel: string;
  onOpenSettings: () => void;
}) {
  return (
    <aside className="flex w-[280px] shrink-0 flex-col bg-sidebar text-primary">
      <div className="flex h-10 shrink-0 items-center justify-end pr-1">
        <button
          type="button"
          aria-label="折叠侧栏"
          title="折叠侧栏"
          onClick={onCollapse}
          className="flex h-7 w-7 items-center justify-center rounded-sm text-secondary transition-colors duration-100 hover:bg-hover hover:text-primary"
        >
          <PanelLeftIcon className="h-4 w-4" />
        </button>
      </div>

      <div className="flex h-[60px] shrink-0 items-center px-2">
        {/* dsh .brandName：18px/24px fw600 letter-spacing .04em（SidebarRoot.module.css:294-304） */}
        <span className="text-[18px] leading-6 font-semibold tracking-[.04em] text-primary">
          SHANIU
        </span>
      </div>

      <div className="flex shrink-0 flex-col gap-1 px-2">
        {/* dsh .newSession：h38、border 0.5px l3、r12、bg button-elevated-fill、14/22 fw500 */}
        <button
          type="button"
          onClick={() => startNewSession(store, transport)}
          title="新建会话"
          className="flex h-[38px] items-center gap-2 rounded-md border-[0.5px] border-line3 bg-btn-elevated px-3 text-[14px] leading-[22px] font-medium text-primary transition-colors duration-100 hover:bg-btn-elevated-hover"
        >
          <PlusIcon className="h-4 w-4 text-secondary" />
          新会话
        </button>
      </div>

      {/* dsh .sectionHeader：h36、label-tertiary（WorkspaceBrowser.module.css:66-79） */}
      <div className="mt-1 flex h-9 shrink-0 items-center px-3 text-[13px] leading-5 text-tertiary">
        工作区
      </div>
      <div className="shrink-0 px-2">
        <button
          type="button"
          onClick={onOpenSettings}
          title={workspaceLabel || '未设置工作区，点击打开设置'}
          className="flex h-9 w-full items-center gap-2 rounded-md px-2 text-left text-[14px] leading-5 text-secondary transition-colors duration-100 hover:bg-hover hover:text-primary"
        >
          <FolderIcon className="h-4 w-4 shrink-0" />
          <span className="min-w-0 truncate">{workspaceLabel || '未设置工作区'}</span>
        </button>
      </div>

      <div className="min-h-0 flex-1" />

      <div className="shrink-0 px-2 pb-2">
        <button
          type="button"
          aria-label="打开设置"
          title="设置"
          onClick={onOpenSettings}
          className="flex h-9 w-full items-center gap-2 rounded-md px-2 text-[14px] leading-5 text-secondary transition-colors duration-100 hover:bg-hover hover:text-primary"
        >
          <GearIcon className="h-4 w-4 shrink-0" />
          设置
        </button>
      </div>
    </aside>
  );
}

/** 左侧栏·折叠轨（rail 56px：36×36 展开钮）。 */
export function SidebarRail({ onExpand }: { onExpand: () => void }) {
  return (
    <aside className="flex w-14 shrink-0 flex-col items-center bg-sidebar pt-[18px]">
      <button
        type="button"
        aria-label="展开侧栏"
        title="展开侧栏"
        onClick={onExpand}
        className="flex h-9 w-9 items-center justify-center rounded-md text-primary transition-colors duration-100 hover:bg-hover"
      >
        <PanelLeftIcon className="h-5 w-5" />
      </button>
    </aside>
  );
}
