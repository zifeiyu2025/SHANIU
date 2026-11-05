// 图标出口（唯一一处）。两类来源分开，理由不同：
//
// 1) 通用 chrome 图标 → lucide-react（ISC、React 19 兼容、sideEffects:false 可 tree-shake）。
//    dsh 的图标集本就是这套线稿语言（24 viewBox，regular 1px / medium 1.3px 描边，
//    见其 artwork.ts:9,12），所以描边统一锁 1.3 对齐 medium 档。保留本文件作为唯一出口，
//    是让调用点不感知库，日后换实现只改这里。
// 2) 输入卡主按钮的三个字形（发送 / 停止 / 在途）→ 内联 SVG，照抄 dsh InputBar.tsx:563-577。
//    这三个是品牌填充字形（16 viewBox、fill=currentColor），不是线稿图标，
//    库里没有对应物，硬套反而偏离参照实现。
import {
  ChevronDown,
  ChevronUp,
  Columns2,
  Compass,
  Copy,
  Check,
  Database,
  Folder,
  GitBranch,
  Globe,
  LayoutGrid,
  ListOrdered,
  Maximize,
  Minus,
  PanelLeft,
  PanelRight,
  Plus,
  RefreshCw,
  Settings,
  ShieldAlert,
  Terminal,
  Trash2,
  X,
  type LucideIcon,
} from 'lucide-react';

interface IconProps {
  className?: string;
}

/** lucide 图标 → 我们的命名出口，描边锁 dsh 的 medium 档 1.3px。 */
function line(Cmp: LucideIcon) {
  return function Icon({ className }: IconProps) {
    return <Cmp className={className} strokeWidth={1.3} aria-hidden />;
  };
}

export const PanelLeftIcon = line(PanelLeft);
export const PanelRightIcon = line(PanelRight);
export const ColumnsIcon = line(Columns2);
export const RefreshIcon = line(RefreshCw);
export const CompassIcon = line(Compass);
export const TerminalIcon = line(Terminal);
export const GlobeIcon = line(Globe);
export const GridIcon = line(LayoutGrid);
export const MinusIcon = line(Minus);
export const MaximizeIcon = line(Maximize);
export const CloseIcon = line(X);
export const PlusIcon = line(Plus);
export const ChevronDownIcon = line(ChevronDown);
export const ChevronUpIcon = line(ChevronUp);
export const FolderIcon = line(Folder);
export const DatabaseIcon = line(Database);
export const WarningIcon = line(ShieldAlert);
export const TrashIcon = line(Trash2);
export const QueueIcon = line(ListOrdered);
export const GearIcon = line(Settings);
export const CopyIcon = line(Copy);
export const CheckIcon = line(Check);
export const BranchIcon = line(GitBranch);

/** 发送：dsh 的填充上箭头字形（InputBar.tsx:576，16 viewBox）。 */
export function ArrowUpIcon({ className }: IconProps) {
  return (
    <svg viewBox="0 0 16 16" width="16" height="16" className={className} aria-hidden>
      <path
        d="M8.3125 0.980183C8.66767 1.0531 8.97902 1.20418 9.2627 1.43233C9.48724 1.61297 9.73029 1.85793 9.97949 2.10714L14.707 6.83468L13.293 8.24874L9 3.95577V15.0417H7V3.95577L2.70703 8.24874L1.29297 6.83468L6.02051 2.10714C6.26971 1.85793 6.51277 1.61297 6.7373 1.43233C6.97662 1.23986 7.28445 1.04402 7.6875 0.980183C7.8973 0.947006 8.1031 0.95516 8.3125 0.980183Z"
        fill="currentColor"
      />
    </svg>
  );
}

/** 停止：16 viewBox 内 10×10 圆角实心方块（dsh InputBar.tsx:564-565，rx=3）。 */
export function StopIcon({ className }: IconProps) {
  return (
    <svg viewBox="0 0 16 16" width="16" height="16" className={className} aria-hidden>
      <rect x="3" y="3" width="10" height="10" rx="3" fill="currentColor" />
    </svg>
  );
}

/** 提交在途：整圈 0.35 透明度 + 90° 亮弧（dsh InputBar.tsx:568-571），旋转由调用方 animate-spin 驱动。 */
export function SpinnerIcon({ className }: IconProps) {
  return (
    <svg viewBox="0 0 16 16" width="16" height="16" className={className} aria-hidden>
      <circle cx="8" cy="8" r="6" fill="none" stroke="currentColor" strokeOpacity="0.35" strokeWidth="2" />
      <path d="M8 2a6 6 0 0 1 6 6" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" />
    </svg>
  );
}
