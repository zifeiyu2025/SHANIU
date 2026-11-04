import type { ReactNode } from 'react';
import { BrandMark, ChevronDownIcon, FolderIcon } from '../icons';

/** 空态标题行（复刻 dsh HeroShell：34px 品牌 + 26/32 wt500 + mono 预览徽章，:29-76）。 */
export function EmptyHero() {
  return (
    <div data-testid="empty-hero" className="flex items-center justify-center gap-2.5">
      <BrandMark className="h-[34px] w-[34px] text-primary" />
      <h1 className="text-[26px] font-medium leading-8 text-primary">把开发任务交给 Agent</h1>
      <span className="mt-1 rounded-full border-[0.5px] border-accent/25 bg-accent/15 px-[7px] pt-px font-mono text-[12px] leading-[18px] font-medium text-primary">
        预览版
      </span>
    </div>
  );
}

/** 工作区选择 chip 行（对齐 dsh：h28、r8、13/20 wt500、folder16+chevron12）。 */
export function SelectorChips() {
  return (
    <div className="flex items-center gap-2">
      <Chip icon={<FolderIcon className="h-4 w-4" />} label="默认工作区" milestone="M1 接入" />
    </div>
  );
}

function Chip({
  icon,
  label,
  milestone,
}: {
  icon: ReactNode;
  label: string;
  milestone: string;
}) {
  return (
    <button
      type="button"
      title={milestone}
      className="flex h-7 items-center gap-1.5 rounded-sm px-2 text-[13px] font-medium leading-5 text-primary transition-colors duration-100 hover:bg-hover"
    >
      {icon}
      {label}
      <ChevronDownIcon className="h-3 w-3 text-caption" />
    </button>
  );
}
