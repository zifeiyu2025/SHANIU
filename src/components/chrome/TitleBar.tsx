import { getCurrentWindow } from '@tauri-apps/api/window';
import { CloseIcon, MaximizeIcon, MinusIcon, PanelLeftIcon, PanelRightIcon } from '../icons';

const isTauri = typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;

/**
 * 无边框窗口自定义标题栏（复刻 dsh）：左侧折叠侧栏 + IPC 状态点；右侧工具面板开关 +
 * 自绘窗口控制按钮；整条可拖拽（data-tauri-drag-region，仅 Tauri 环境生效）。
 */
export function TitleBar({
  collapsed,
  onToggleCollapse,
  rightOpen,
  onToggleRight,
  link,
}: {
  collapsed: boolean;
  onToggleCollapse: () => void;
  rightOpen: boolean;
  onToggleRight: () => void;
  link: 'pending' | 'ok' | 'fail';
}) {
  const win = isTauri ? getCurrentWindow() : null;
  const linkTitle = link === 'ok' ? 'IPC 已连通' : link === 'fail' ? 'IPC 未连通' : 'IPC 连接中';
  return (
    <div
      data-tauri-drag-region
      className="flex h-11 shrink-0 select-none items-center justify-between bg-sidebar pr-1 text-secondary"
    >
      <div className="flex items-center gap-1 pl-2">
        <button
          type="button"
          aria-label={collapsed ? '展开侧栏' : '折叠侧栏'}
          title={collapsed ? '展开侧栏' : '折叠侧栏'}
          onClick={onToggleCollapse}
          className="flex h-8 w-8 items-center justify-center rounded-lg hover:bg-hover hover:text-text"
        >
          <PanelLeftIcon className="h-[18px] w-[18px]" />
        </button>
        <span
          data-testid="link-state"
          title={linkTitle}
          aria-label={linkTitle}
          className={`ml-1 h-2 w-2 rounded-full ${
            link === 'ok' ? 'bg-emerald-500' : link === 'fail' ? 'bg-red-500' : 'bg-zinc-600'
          }`}
        />
      </div>

      <div className="flex items-center">
        <button
          type="button"
          aria-label={rightOpen ? '关闭工具面板' : '打开工具面板'}
          title={rightOpen ? '关闭工具面板' : '打开工具面板'}
          onClick={onToggleRight}
          className="mr-1 flex h-8 w-8 items-center justify-center rounded-lg hover:bg-hover hover:text-text"
        >
          <PanelRightIcon className="h-[18px] w-[18px]" />
        </button>
        {win && (
          <>
            <button
              type="button"
              aria-label="最小化"
              onClick={() => void win.minimize()}
              className="flex h-11 w-11 items-center justify-center rounded-md hover:bg-hover hover:text-text"
            >
              <MinusIcon className="h-4 w-4" />
            </button>
            <button
              type="button"
              aria-label="最大化"
              onClick={() => void win.toggleMaximize()}
              className="flex h-11 w-11 items-center justify-center rounded-md hover:bg-hover hover:text-text"
            >
              <MaximizeIcon className="h-3.5 w-3.5" />
            </button>
            <button
              type="button"
              aria-label="关闭"
              onClick={() => void win.close()}
              className="flex h-11 w-11 items-center justify-center rounded-md hover:bg-red-600 hover:text-white"
            >
              <CloseIcon className="h-4 w-4" />
            </button>
          </>
        )}
      </div>
    </div>
  );
}
