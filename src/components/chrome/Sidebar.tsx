import { PanelLeftIcon } from '../icons';

/**
 * 左侧栏（280px，bg=侧栏填充）：内容区当前为空——
 * 会话列表/工具入口等后续需要时参考 dsh（SidebarRoot/WorkspaceBrowser/Rows）补齐。
 */
export function Sidebar({ onCollapse }: { onCollapse: () => void }) {
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
      <div className="min-h-0 flex-1" />
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
