import type { ReactNode } from 'react';
import type { SessionStore } from '../../store/session-store';
import {
  BrandMark,
  DotsIcon,
  FolderIcon,
  GearIcon,
  PlusIcon,
  PuzzleIcon,
  SearchIcon,
} from '../icons';

/** 左侧栏（复刻 dsh）：品牌行 / 新会话 / 菜单 / 工作区分组 / 底部用户。 */
export function Sidebar({ store, onNewSession }: { store: SessionStore; onNewSession: () => void }) {
  const nodeCount = 0; // 占位：M3 会话列表接入后替换为真实会话列表
  void nodeCount;
  void store;

  return (
    <aside className="flex w-[280px] shrink-0 flex-col bg-sidebar text-[13px]">
      {/* 品牌行 */}
      <div className="flex items-center gap-2 px-4 pt-3 pb-2">
        <BrandMark className="h-6 w-6 text-text" />
        <span className="text-[15px] font-semibold tracking-wide text-white">SHANIU</span>
        <span className="rounded-full border border-secondary/70 px-1.5 py-px text-[9px] tracking-widest text-secondary">
          HARNESS
        </span>
      </div>

      {/* 新会话 */}
      <div className="px-3 pt-1">
        <button
          type="button"
          onClick={onNewSession}
          className="flex h-10 w-full items-center justify-center gap-2 rounded-[10px] bg-hover text-text transition-colors hover:bg-raised"
        >
          <PlusIcon className="h-4 w-4" />
          新会话
        </button>
      </div>

      {/* 菜单（M1+ 接入对应功能，当前仅占位） */}
      <nav className="mt-3 flex flex-col gap-0.5 px-3">
        {[
          { icon: GearIcon, label: '设置', milestone: 'M4' },
          { icon: PuzzleIcon, label: '插件', milestone: '二期' },
          { icon: DotsIcon, label: '更多', milestone: '二期' },
        ].map(({ icon: Icon, label, milestone }) => (
          <button
            key={label}
            type="button"
            title={`${milestone}接入`}
            className="flex h-9 items-center gap-2.5 rounded-lg px-2.5 text-secondary hover:bg-hover hover:text-text"
          >
            <Icon className="h-4 w-4" />
            {label}
          </button>
        ))}
      </nav>

      {/* 工作区分组 */}
      <div className="mt-5 flex min-h-0 flex-1 flex-col">
        <div className="flex items-center justify-between px-4 pb-1">
          <span className="text-xs text-weak">工作区</span>
          <button
            type="button"
            title="搜索工作区（M3 接入）"
            className="flex h-6 w-6 items-center justify-center rounded-md text-weak hover:bg-hover hover:text-secondary"
          >
            <SearchIcon className="h-3.5 w-3.5" />
          </button>
        </div>
        <div className="flex-1 overflow-y-auto px-2">
          <SidebarItem icon={<FolderIcon className="h-4 w-4" />} label="默认工作区" />
          <SidebarItem label="新会话" selected />
        </div>
      </div>

      {/* 底部用户 */}
      <div className="flex items-center gap-2.5 px-4 py-3">
        <span className="flex h-7 w-7 items-center justify-center rounded-full bg-accent/30 text-xs font-semibold text-text">
          本
        </span>
        <span className="text-secondary">本地用户</span>
      </div>
    </aside>
  );
}

function SidebarItem({
  icon,
  label,
  selected = false,
}: {
  icon?: ReactNode;
  label: string;
  selected?: boolean;
}) {
  return (
    <button
      type="button"
      className={`flex h-9 w-full items-center gap-2.5 rounded-lg px-2.5 ${
        selected ? 'bg-raised text-white' : 'text-secondary hover:bg-hover hover:text-text'
      }`}
    >
      {icon}
      <span className="truncate">{label}</span>
    </button>
  );
}
