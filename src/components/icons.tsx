// 极简线条图标（复刻 dsh 的 lucide 风格线稿图标，内联 SVG 免依赖）
interface IconProps {
  className?: string;
}

function base(className?: string) {
  return {
    viewBox: '0 0 24 24',
    fill: 'none',
    stroke: 'currentColor',
    strokeWidth: 1.3, // dsh 图标两档描边：regular 1px / medium 1.3px（artwork.ts:9,12）
    strokeLinecap: 'round' as const,
    strokeLinejoin: 'round' as const,
    className,
    'aria-hidden': true,
  };
}

export function PanelLeftIcon({ className }: IconProps) {
  return (
    <svg {...base(className)}>
      <rect x="3" y="4" width="18" height="16" rx="2.5" />
      <line x1="9.5" y1="4" x2="9.5" y2="20" />
    </svg>
  );
}

export function PanelRightIcon({ className }: IconProps) {
  return (
    <svg {...base(className)}>
      <rect x="3" y="4" width="18" height="16" rx="2.5" />
      <line x1="15" y1="4" x2="15" y2="20" />
    </svg>
  );
}

export function ColumnsIcon({ className }: IconProps) {
  return (
    <svg {...base(className)}>
      <rect x="3" y="4" width="18" height="16" rx="2.5" />
      <line x1="12" y1="4" x2="12" y2="20" />
    </svg>
  );
}

export function RefreshIcon({ className }: IconProps) {
  return (
    <svg {...base(className)}>
      <path d="M20 11.5a8 8 0 1 1-2.34-5.66" />
      <polyline points="20,4 20,8 16,8" />
    </svg>
  );
}

export function CompassIcon({ className }: IconProps) {
  return (
    <svg {...base(className)}>
      <circle cx="12" cy="12" r="8.5" />
      <polygon points="15.5,8.5 13.5,13.5 8.5,15.5 10.5,10.5" fill="currentColor" stroke="none" />
    </svg>
  );
}

export function TerminalIcon({ className }: IconProps) {
  return (
    <svg {...base(className)}>
      <polyline points="5,8 9.5,12 5,16" />
      <line x1="12.5" y1="16" x2="18.5" y2="16" />
    </svg>
  );
}

export function GlobeIcon({ className }: IconProps) {
  return (
    <svg {...base(className)}>
      <circle cx="12" cy="12" r="8.5" />
      <ellipse cx="12" cy="12" rx="3.8" ry="8.5" />
      <line x1="3.5" y1="12" x2="20.5" y2="12" />
    </svg>
  );
}

export function GridIcon({ className }: IconProps) {
  return (
    <svg {...base(className)}>
      <rect x="4" y="4" width="6.5" height="6.5" rx="1.5" />
      <rect x="13.5" y="4" width="6.5" height="6.5" rx="1.5" />
      <rect x="4" y="13.5" width="6.5" height="6.5" rx="1.5" />
      <rect x="13.5" y="13.5" width="6.5" height="6.5" rx="1.5" />
    </svg>
  );
}

export function MinusIcon({ className }: IconProps) {
  return (
    <svg {...base(className)}>
      <line x1="5.5" y1="12" x2="18.5" y2="12" />
    </svg>
  );
}

export function MaximizeIcon({ className }: IconProps) {
  return (
    <svg {...base(className)}>
      <rect x="5.5" y="5.5" width="13" height="13" rx="2" />
    </svg>
  );
}

export function CloseIcon({ className }: IconProps) {
  return (
    <svg {...base(className)}>
      <line x1="6" y1="6" x2="18" y2="18" />
      <line x1="18" y1="6" x2="6" y2="18" />
    </svg>
  );
}

export function PlusIcon({ className }: IconProps) {
  return (
    <svg {...base(className)}>
      <line x1="12" y1="5.5" x2="12" y2="18.5" />
      <line x1="5.5" y1="12" x2="18.5" y2="12" />
    </svg>
  );
}

export function ChevronDownIcon({ className }: IconProps) {
  return (
    <svg {...base(className)}>
      <polyline points="6,9.5 12,15.5 18,9.5" />
    </svg>
  );
}

export function FolderIcon({ className }: IconProps) {
  return (
    <svg {...base(className)}>
      <path d="M3.5 7a2 2 0 0 1 2-2h4l2.2 2.5h7.3a1.5 1.5 0 0 1 1.5 1.5v8a2 2 0 0 1-2 2h-13a2 2 0 0 1-2-2z" />
    </svg>
  );
}

export function ShieldIcon({ className }: IconProps) {
  return (
    <svg {...base(className)}>
      <path d="M12 3.5 5 6v5.2c0 4.4 2.9 7.6 7 9.3 4.1-1.7 7-4.9 7-9.3V6z" />
    </svg>
  );
}

/** 数据库图标（对齐 dsh IconDatabaseOutlineRegular，用量胶囊用）。 */
export function DatabaseIcon({ className }: IconProps) {
  return (
    <svg {...base(className)}>
      <ellipse cx="12" cy="6" rx="7" ry="2.8" />
      <path d="M5 6v12c0 1.55 3.13 2.8 7 2.8s7-1.25 7-2.8V6" />
      <path d="M5 12c0 1.55 3.13 2.8 7 2.8s7-1.25 7-2.8" />
    </svg>
  );
}

export function WarningIcon({ className }: IconProps) {
  return (
    <svg {...base(className)}>
      <circle cx="12" cy="12" r="8.5" />
      <line x1="12" y1="7.6" x2="12" y2="12.8" />
      <circle cx="12" cy="15.9" r="0.9" fill="currentColor" stroke="none" />
    </svg>
  );
}

export function ArrowUpIcon({ className }: IconProps) {
  return (
    <svg {...base(className)}>
      <line x1="12" y1="19" x2="12" y2="5.5" />
      <polyline points="6,11 12,5 18,11" />
    </svg>
  );
}

export function StopIcon({ className }: IconProps) {
  return (
    <svg {...base(className)}>
      <rect x="7" y="7" width="10" height="10" rx="1.5" fill="currentColor" stroke="none" />
    </svg>
  );
}

/** 待发队列（dsh IconQueueOutlineRegular 同形：三条右对齐横线 + 引导线）。 */
export function QueueIcon({ className }: IconProps) {
  return (
    <svg {...base(className)}>
      <line x1="9" y1="6" x2="20" y2="6" />
      <line x1="9" y1="12" x2="20" y2="12" />
      <line x1="9" y1="18" x2="20" y2="18" />
      <circle cx="4.5" cy="6" r="1" fill="currentColor" stroke="none" />
      <circle cx="4.5" cy="12" r="1" fill="currentColor" stroke="none" />
      <circle cx="4.5" cy="18" r="1" fill="currentColor" stroke="none" />
    </svg>
  );
}

export function TrashIcon({ className }: IconProps) {
  return (
    <svg {...base(className)}>
      <line x1="4" y1="7" x2="20" y2="7" />
      <path d="M6.5 7v12a1.5 1.5 0 0 0 1.5 1.5h8a1.5 1.5 0 0 0 1.5-1.5V7" />
      <path d="M9.5 7V4.5A1.5 1.5 0 0 1 11 3h2a1.5 1.5 0 0 1 1.5 1.5V7" />
    </svg>
  );
}

/** 发送中：整圈淡、局部深的旋转弧（dsh 用 Spinner，同形态）。 */
export function SpinnerIcon({ className }: IconProps) {
  return (
    <svg {...base(className)}>
      <circle cx="12" cy="12" r="8" opacity="0.25" />
      <path d="M20 12a8 8 0 0 0-8-8" />
    </svg>
  );
}

export function ChevronUpIcon({ className }: IconProps) {
  return (
    <svg {...base(className)}>
      <polyline points="6,15 12,9 18,15" />
    </svg>
  );
}

export function SearchIcon({ className }: IconProps) {
  return (
    <svg {...base(className)}>
      <circle cx="11" cy="11" r="6" />
      <line x1="15.5" y1="15.5" x2="19.5" y2="19.5" />
    </svg>
  );
}

export function GearIcon({ className }: IconProps) {
  return (
    <svg {...base(className)}>
      <circle cx="12" cy="12" r="3.2" />
      <path d="M12 3.8v2.4M12 17.8v2.4M4.6 8.1l2 1.2M17.4 14.7l2 1.2M4.6 15.9l2-1.2M17.4 9.3l2-1.2" />
    </svg>
  );
}

export function PuzzleIcon({ className }: IconProps) {
  return (
    <svg {...base(className)}>
      <path d="M10 4.5a2 2 0 1 1 4 0V6h3a1.5 1.5 0 0 1 1.5 1.5v3H20a2 2 0 1 1 0 4h-1.5v3A1.5 1.5 0 0 1 17 19h-3v1.5a2 2 0 1 1-4 0V19H7a1.5 1.5 0 0 1-1.5-1.5v-3H4a2 2 0 1 1 0-4h1.5v-3A1.5 1.5 0 0 1 7 6h3z" />
    </svg>
  );
}

export function DotsIcon({ className }: IconProps) {
  return (
    <svg {...base(className)}>
      <circle cx="5" cy="12" r="1.4" fill="currentColor" stroke="none" />
      <circle cx="12" cy="12" r="1.4" fill="currentColor" stroke="none" />
      <circle cx="19" cy="12" r="1.4" fill="currentColor" stroke="none" />
    </svg>
  );
}

/** SHANIU 品牌字标（几何 S 印章，替代 dsh 的鲸鱼 logo） */
export function BrandMark({ className }: IconProps) {
  return (
    <svg viewBox="0 0 24 24" className={className} aria-hidden fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
      <rect x="3" y="3" width="18" height="18" rx="5" />
      <path d="M15.5 8.5c-1-1.2-2.3-1.7-3.7-1.5-1.6.2-2.6 1.3-2.4 2.6.2 1.2 1.1 1.8 3 2.1 2.2.4 3.4 1.1 3.6 2.6.2 1.6-1.1 2.9-3 3.1-1.7.2-3.2-.5-4.1-1.9" />
    </svg>
  );
}
