import type { ComponentType, ReactNode } from 'react';
import {
  DotsIcon,
  FolderIcon,
  GearIcon,
  PanelLeftIcon,
  PlusIcon,
  PuzzleIcon,
  SearchIcon,
} from '../icons';

const ICON_BTN_28 =
  'flex h-7 w-7 items-center justify-center rounded-sm text-tertiary transition-colors duration-100 hover:bg-hover hover:text-secondary';

const MENU_ROWS: Array<{ icon: ComponentType<{ className?: string }>; label: string; milestone: string }> = [
  { icon: GearIcon, label: '设置', milestone: 'M4 接入' },
  { icon: PuzzleIcon, label: '插件', milestone: '二期接入' },
  { icon: DotsIcon, label: '更多', milestone: '二期接入' },
];

/** 左侧栏·展开态（复刻 dsh SidebarRoot 几何：280px、行 32px；品牌区/用户区按需求移除）。 */
export function Sidebar({
  onNewSession,
  onCollapse,
}: {
  onNewSession: () => void;
  onCollapse: () => void;
}) {
  return (
    <aside className="flex w-[280px] shrink-0 flex-col bg-sidebar pb-2 text-primary">
      {/* 折叠钮行（dsh 折叠钮位于 logo 行右侧；品牌区按需求移除） */}
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

      {/* 新会话：h38、r12、0.5px 边框、elevated 填充（:420-531） */}
      <button
        type="button"
        onClick={onNewSession}
        className="mx-1 flex h-[38px] shrink-0 items-center justify-center gap-2 rounded-md border-[0.5px] border-line3 bg-btn-elevated text-[14px] font-medium leading-[22px] transition-colors duration-100 hover:bg-btn-elevated-hover"
      >
        <PlusIcon className="h-3.5 w-3.5" />
        新会话
      </button>

      {/* 菜单行：min-h 32、pad 5 9、r12、label-primary（:685-724） */}
      <nav className="mt-2 flex shrink-0 flex-col">
        {MENU_ROWS.map(({ icon: Icon, label, milestone }) => (
          <button
            key={label}
            type="button"
            title={milestone}
            className="flex min-h-8 items-center gap-1.5 rounded-md px-[9px] py-[5px] text-left text-[14px] leading-[22px] transition-colors duration-100 hover:bg-hover"
          >
            <Icon className="h-4 w-4" />
            {label}
          </button>
        ))}
      </nav>

      {/* 工作区分组：标题行 36px tertiary（WorkspaceBrowser.module.css:66-79） */}
      <div className="mt-1 flex min-h-0 flex-1 flex-col">
        <div className="flex h-9 shrink-0 items-center justify-between pl-3 pr-0.5">
          <span className="text-tertiary">工作区</span>
          <button type="button" title="搜索工作区（M3 接入）" className={ICON_BTN_28}>
            <SearchIcon className="h-3.5 w-3.5" />
          </button>
        </div>
        <div className="relative min-h-0 flex-1 overflow-y-auto pr-1">
          <SidebarRow icon={<FolderIcon className="h-4 w-4" />} label="默认工作区" />
          <SidebarRow label="新会话" selected />
          {/* 列表底部 24px 渐隐（:586-593） */}
          <div className="pointer-events-none sticky bottom-0 -mb-1 h-6 bg-gradient-to-t from-sidebar to-transparent" />
        </div>
      </div>
    </aside>
  );
}

/** 左侧栏·折叠轨（rail 56px：36×36 展开钮，:337-394）。 */
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

function SidebarRow({ icon, label, selected = false }: { icon?: ReactNode; label: string; selected?: boolean }) {
  return (
    <button
      type="button"
      className={`flex h-8 w-full items-center gap-2 rounded-md px-2 text-left text-[14px] leading-5 transition-colors duration-100 hover:bg-hover ${
        selected ? 'bg-hover' : ''
      }`}
    >
      {icon}
      <span className="truncate">{label}</span>
    </button>
  );
}
