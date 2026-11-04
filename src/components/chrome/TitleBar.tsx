import { getCurrentWindow } from '@tauri-apps/api/window';
import { CloseIcon, MaximizeIcon, MinusIcon, PanelRightIcon } from '../icons';

const isTauri = typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;

const MENU_BUTTON =
  'flex h-7 items-center rounded-[6px] px-2.5 text-[14px] leading-5 text-secondary ' +
  'transition-colors duration-100 hover:bg-hover hover:text-primary';
const WIN_BUTTON =
  'flex h-10 w-11 items-center justify-center text-secondary transition-colors duration-100 hover:bg-hover hover:text-primary';

/**
 * 窗口标题栏（复刻 dsh Windows chrome：40px 拖拽带，windows-layout.ts:4）。
 * 左：IPC 状态点 + 应用/编辑菜单位（dsh 为原生菜单，M1 接入；侧栏折叠钮在侧栏 logo 行）；
 * 右：工具面板开关 + 自绘窗口控制（Tauri decorations:false；close hover #e81123）。
 */
export function TitleBar({
  rightOpen,
  onToggleRight,
  link,
}: {
  rightOpen: boolean;
  onToggleRight: () => void;
  link: 'pending' | 'ok' | 'fail';
}) {
  const win = isTauri ? getCurrentWindow() : null;
  const linkTitle = link === 'ok' ? 'IPC 已连通' : link === 'fail' ? 'IPC 未连通' : 'IPC 连接中';

  return (
    <div
      data-tauri-drag-region
      className="flex h-10 shrink-0 select-none items-center justify-between bg-sidebar pl-2.5"
    >
      <div className="flex items-center gap-0.5">
        <span
          data-testid="link-state"
          title={linkTitle}
          aria-label={linkTitle}
          className={`ml-1 mr-1 h-2 w-2 rounded-full ${
            link === 'ok' ? 'bg-success' : link === 'fail' ? 'bg-danger' : 'bg-dimmed'
          }`}
        />
        {['应用', '编辑'].map((menu) => (
          <button key={menu} type="button" title={`${menu}菜单（M1 接入）`} className={MENU_BUTTON}>
            {menu}
          </button>
        ))}
      </div>

      <div className="flex items-center">
        <button
          type="button"
          aria-label={rightOpen ? '关闭工具面板' : '打开工具面板'}
          title={rightOpen ? '关闭工具面板' : '打开工具面板'}
          onClick={onToggleRight}
          className="mr-1 flex h-7 w-7 items-center justify-center rounded-sm text-secondary transition-colors duration-100 hover:bg-hover hover:text-primary"
        >
          <PanelRightIcon className="h-4 w-4" />
        </button>
        {win && (
          <>
            <button type="button" aria-label="最小化" onClick={() => void win.minimize()} className={WIN_BUTTON}>
              <MinusIcon className="h-4 w-4" />
            </button>
            <button
              type="button"
              aria-label="最大化"
              onClick={() => void win.toggleMaximize()}
              className={WIN_BUTTON}
            >
              <MaximizeIcon className="h-3.5 w-3.5" />
            </button>
            <button
              type="button"
              aria-label="关闭"
              onClick={() => void win.close()}
              className={`${WIN_BUTTON} hover:bg-[--dsw-close-hover] hover:text-white`}
            >
              <CloseIcon className="h-4 w-4" />
            </button>
          </>
        )}
      </div>
    </div>
  );
}
