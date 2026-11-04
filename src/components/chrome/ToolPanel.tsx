import { Fragment, type ComponentType } from 'react';
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

// 复刻 dsh「开始」工具面板（GuideBody）；M0 为占位入口，按里程碑接入真实功能。
const TOOLS: ToolCard[] = [
  {
    icon: FolderIcon,
    tone: 'text-tertiary',
    title: '工作区文件',
    subtitle: '浏览会话工作区的文件',
    shortcut: 'Ctrl + P',
    milestone: 'M1 接入',
  },
  {
    icon: TerminalIcon,
    tone: 'text-tertiary',
    title: '新建终端',
    subtitle: '在会话工作区运行命令',
    shortcut: 'Ctrl + `',
    milestone: 'M1 接入',
  },
  {
    icon: GlobeIcon,
    tone: 'text-tertiary',
    title: '浏览器',
    subtitle: '浏览网页',
    shortcut: 'Ctrl + T',
    milestone: '二期接入',
  },
  {
    icon: GridIcon,
    tone: 'text-tertiary',
    title: '更多工具',
    milestone: '二期接入',
  },
];

const HEADER_BTN =
  'flex h-7 w-7 items-center justify-center rounded-sm text-secondary transition-colors duration-100 hover:bg-hover hover:text-primary';

/** 右侧工具面板（复刻 dsh dockkit + GuideBody）：quiet tab 头 + 56px 指南针 + 380×56 r20 工具卡。 */
export function ToolPanel({ onClose }: { onClose: () => void }) {
  return (
    <aside
      data-testid="tool-panel"
      className="flex w-[45%] min-w-[300px] max-w-[70%] shrink-0 flex-col border-l-[0.5px] border-line4 bg-canvas"
    >
      {/* 头部：quiet tab chip（primary 无底）+ 右侧 28×28 图标钮（dockkit:504-533） */}
      <div className="flex h-[42px] shrink-0 items-center justify-between pl-3 pr-2">
        <span className="flex items-center gap-1.5 px-1 text-[13px] leading-5 font-medium text-primary">
          <CompassIcon className="h-[15px] w-[15px]" />
          开始
        </span>
        <div className="flex items-center gap-0.5">
          <button type="button" title="刷新（M1 接入）" className={HEADER_BTN}>
            <RefreshIcon className="h-[15px] w-[15px]" />
          </button>
          <button type="button" title="布局（M1 接入）" className={HEADER_BTN}>
            <ColumnsIcon className="h-[15px] w-[15px]" />
          </button>
          <button type="button" aria-label="关闭工具面板" title="关闭工具面板" onClick={onClose} className={HEADER_BTN}>
            <PanelRightIcon className="h-[15px] w-[15px]" />
          </button>
        </div>
      </div>

      {/* 空态：56px 指南针（neutral-700）+ 工具卡列（gap14、pad 0 24、底部 10% 上浮） */}
      <div className="flex min-h-0 flex-1 flex-col items-center justify-center gap-3.5 px-6 pb-[10%]">
        <CompassIcon className="mb-3 h-14 w-14 text-[color:var(--dsw-neutral-700)]" />
        <div className="flex w-[380px] max-w-full flex-col gap-3.5">
          {TOOLS.map(({ icon: Icon, tone, title, subtitle, shortcut, milestone }) => (
            <button
              key={title}
              type="button"
              title={milestone}
              className="flex min-h-[56px] w-full items-center gap-3.5 rounded-xl border-[0.5px] border-line3 bg-layer1 px-5 py-3 text-left transition-colors duration-100 hover:bg-hover"
            >
              <Icon className={`h-5 w-5 shrink-0 ${tone}`} />
              <span className="min-w-0 flex-1">
                <span className="block truncate text-[14px] leading-[1.4] text-primary">{title}</span>
                {subtitle && <span className="block truncate text-[11px] leading-[1.4] text-tertiary">{subtitle}</span>}
              </span>
              {shortcut && <ShortcutKeys combo={shortcut} />}
            </button>
          ))}
        </div>
      </div>
    </aside>
  );
}

/** 快捷键 keycap（复刻 dsh ShortcutKeys：h16、r4、bg tooltip-key-bg、11/14）。 */
function ShortcutKeys({ combo }: { combo: string }) {
  const keys = combo
    .split('+')
    .map((k) => k.trim())
    .filter(Boolean);
  return (
    <span className="flex shrink-0 items-center gap-1 text-tertiary">
      {keys.map((key, index) => (
        <Fragment key={key}>
          {index > 0 && <span className="text-[11px] leading-4">+</span>}
          <kbd className="flex h-4 min-w-4 items-center justify-center rounded-[4px] bg-(--dsw-keycap) px-1 font-sans text-[11px] leading-4 text-primary">
            {key}
          </kbd>
        </Fragment>
      ))}
    </span>
  );
}
