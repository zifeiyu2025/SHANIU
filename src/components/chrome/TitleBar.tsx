import type { MouseEvent } from 'react';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { CloseIcon, MaximizeIcon, MinusIcon, PanelRightIcon } from '../icons';

const isTauri = typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;

/** 拖拽起步阈值（对齐 Windows SM_CXDRAG=4px）：未超阈值前不进入系统移动循环，保住双击。 */
const DRAG_THRESHOLD_PX = 4;

const WIN_BUTTON =
  'flex h-10 w-11 items-center justify-center text-secondary transition-colors duration-100 hover:bg-hover hover:text-primary';

/**
 * 窗口标题栏（复刻 dsh Windows chrome：40px 拖拽带，windows-layout.ts:4）。
 * 无边框窗口移动：mousedown 暂记起点，mousemove 超过阈值才调用 startDragging
 * （立即进入系统移动循环会吞掉双击的第二次点击），双击 = 最大化/还原；
 * 左：IPC 状态点；右：工具面板开关 + 自绘窗口控制（close hover #e81123）。
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

  // 只在空白拖拽区触发；按钮等交互元素自行处理
  function onDragAreaMouseDown(e: MouseEvent<HTMLDivElement>) {
    if (!win || e.button !== 0) return;
    if ((e.target as HTMLElement).closest('button, input, textarea, [role="button"]')) return;

    const startX = e.clientX;
    const startY = e.clientY;
    let started = false;
    const onMove = (ev: globalThis.MouseEvent) => {
      if (started) return;
      // 按键已在别处释放（页面收不到 mouseup 的场景）——放弃本次拖拽
      if (ev.buttons !== 1) {
        cleanup();
        return;
      }
      if (
        Math.abs(ev.clientX - startX) > DRAG_THRESHOLD_PX ||
        Math.abs(ev.clientY - startY) > DRAG_THRESHOLD_PX
      ) {
        started = true;
        cleanup();
        void win.startDragging();
      }
    };
    const onUp = () => cleanup();
    const cleanup = () => {
      window.removeEventListener('mousemove', onMove);
      window.removeEventListener('mouseup', onUp);
    };
    window.addEventListener('mousemove', onMove);
    window.addEventListener('mouseup', onUp);
  }

  function onDragAreaDoubleClick(e: MouseEvent<HTMLDivElement>) {
    if (!win) return;
    if ((e.target as HTMLElement).closest('button')) return;
    void win.toggleMaximize();
  }

  return (
    <div
      onMouseDown={onDragAreaMouseDown}
      onDoubleClick={onDragAreaDoubleClick}
      className="flex h-10 shrink-0 select-none items-center justify-between bg-sidebar pl-2.5"
    >
      <div className="flex items-center">
        <span
          data-testid="link-state"
          title={linkTitle}
          aria-label={linkTitle}
          className={`ml-1 h-2 w-2 rounded-full ${
            link === 'ok' ? 'bg-success' : link === 'fail' ? 'bg-danger' : 'bg-dimmed'
          }`}
        />
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
