import type { MouseEvent } from 'react';
import { useState } from 'react';
import { getCurrentWindow } from '@tauri-apps/api/window';

const isTauri = typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;

/** 与 @tauri-apps/api window.d.ts 的 ResizeDirection 结构一致 */
type ResizeDir = 'East' | 'North' | 'NorthEast' | 'NorthWest' | 'South' | 'SouthEast' | 'SouthWest' | 'West';

const STRIPS: Array<{ dir: ResizeDir; className: string }> = [
  { dir: 'North', className: 'top-0 left-3 right-3 h-1 cursor-[ns-resize]' },
  { dir: 'South', className: 'bottom-0 left-3 right-3 h-1 cursor-[ns-resize]' },
  { dir: 'West', className: 'left-0 top-3 bottom-3 w-1 cursor-[ew-resize]' },
  { dir: 'East', className: 'right-0 top-3 bottom-3 w-1 cursor-[ew-resize]' },
  { dir: 'NorthWest', className: 'left-0 top-0 h-3 w-3 cursor-[nwse-resize]' },
  { dir: 'NorthEast', className: 'right-0 top-0 h-3 w-3 cursor-[nesw-resize]' },
  { dir: 'SouthWest', className: 'bottom-0 left-0 h-3 w-3 cursor-[nesw-resize]' },
  { dir: 'SouthEast', className: 'bottom-0 right-0 h-3 w-3 cursor-[nwse-resize]' },
];

/**
 * 无边框窗口边缘缩放层：tao 不为无边框窗口做边缘 hit-test，
 * 以 8 条隐形热区 + 核心 startResizeDragging(direction) 显式实现（与 startDragging 同族）。
 * 仅 Tauri 环境渲染；边缘 4px、四角 12px，覆盖在内容最上层。
 */
export function EdgeResizeLayer() {
  const [win] = useState(() => (isTauri ? getCurrentWindow() : null));
  if (!win) return null;

  function onMouseDown(e: MouseEvent<HTMLDivElement>, dir: ResizeDir) {
    if (e.button !== 0) return;
    void win!.startResizeDragging(dir);
  }

  return (
    <>
      {STRIPS.map(({ dir, className }) => (
        <div
          key={dir}
          onMouseDown={(e) => onMouseDown(e, dir)}
          className={`fixed z-50 ${className}`}
        />
      ))}
    </>
  );
}
