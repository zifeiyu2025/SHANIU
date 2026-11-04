import type { ComponentType } from 'react';
import {
  ColumnsIcon,
  CompassIcon,
  FolderIcon,
  GlobeIcon,
  GridIcon,
  PanelRightIcon,
  RefreshIcon,
  TerminalIcon,
} from '../icons';

interface ToolCard {
  icon: ComponentType<{ className?: string }>;
  tone: string;
  title: string;
  subtitle?: string;
  shortcut?: string;
  milestone: string;
}

// 复刻 dsh「开始」工具面板；M0 为占位入口，按里程碑接入真实功能。
const TOOLS: ToolCard[] = [
  {
    icon: FolderIcon,
    tone: 'text-amber-400',
    title: '工作区文件',
    subtitle: '浏览会话工作区的文件',
    shortcut: 'Ctrl + P',
    milestone: 'M1 接入',
  },
  {
    icon: TerminalIcon,
    tone: 'text-sky-400',
    title: '新建终端',
    subtitle: '在会话工作区运行命令',
    shortcut: 'Ctrl + `',
    milestone: 'M1 接入',
  },
  {
    icon: GlobeIcon,
    tone: 'text-sky-400',
    title: '浏览器',
    subtitle: '浏览网页',
    shortcut: 'Ctrl + T',
    milestone: '二期接入',
  },
  {
    icon: GridIcon,
    tone: 'text-secondary',
    title: '更多工具',
    milestone: '二期接入',
  },
];

/** 右侧工具面板（复刻 dsh）：「开始」头 + 指南针空态 + 工具卡片列。 */
export function ToolPanel({ onClose }: { onClose: () => void }) {
  return (
    <aside
      data-testid="tool-panel"
      className="flex w-[440px] max-w-[46%] shrink-0 flex-col border-l border-border/60 bg-surface text-[13px]"
    >
      <div className="flex items-center justify-between px-4 py-3">
        <span className="flex items-center gap-2 text-text">
          <CompassIcon className="h-4 w-4" />
          开始
        </span>
        <div className="flex items-center gap-1 text-secondary">
          <button
            type="button"
            title="刷新（M1 接入）"
            className="flex h-7 w-7 items-center justify-center rounded-md hover:bg-hover hover:text-text"
          >
            <RefreshIcon className="h-4 w-4" />
          </button>
          <button
            type="button"
            title="布局（M1 接入）"
            className="flex h-7 w-7 items-center justify-center rounded-md hover:bg-hover hover:text-text"
          >
            <ColumnsIcon className="h-4 w-4" />
          </button>
          <button
            type="button"
            aria-label="关闭工具面板"
            onClick={onClose}
            className="flex h-7 w-7 items-center justify-center rounded-md hover:bg-hover hover:text-text"
          >
            <PanelRightIcon className="h-4 w-4" />
          </button>
        </div>
      </div>

      <div className="flex min-h-0 flex-1 flex-col items-center justify-center gap-8 px-12 pb-14">
        <CompassIcon className="h-11 w-11 text-secondary/50" />
        <div className="flex w-full flex-col gap-3.5">
          {TOOLS.map(({ icon: Icon, tone, title, subtitle, shortcut, milestone }) => (
            <button
              key={title}
              type="button"
              title={milestone}
              className="flex w-full items-center gap-3.5 rounded-xl bg-raised px-4 py-3.5 text-left transition-colors hover:bg-hover"
            >
              <Icon className={`h-5 w-5 shrink-0 ${tone}`} />
              <span className="min-w-0 flex-1">
                <span className="block text-text">{title}</span>
                {subtitle && (
                  <span className="block truncate text-xs text-weak">{subtitle}</span>
                )}
              </span>
              {shortcut && <span className="shrink-0 text-xs text-weak">{shortcut}</span>}
            </button>
          ))}
        </div>
      </div>
    </aside>
  );
}
