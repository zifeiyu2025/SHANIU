import { BrandMark, ChevronDownIcon, FolderIcon, ShieldIcon } from '../icons';

/** 会话空态欢迎区（复刻 dsh 空态：品牌语 + 徽章 + 工作区/沙箱选择行）。 */
export function EmptyHero() {
  return (
    <div data-testid="empty-hero" className="flex flex-col items-center gap-7 text-center">
      <div className="flex flex-col items-center gap-3">
        <BrandMark className="h-10 w-10 text-text" />
        <h1 className="flex items-center gap-2 text-[26px] font-bold leading-tight text-white">
          把开发任务交给 Agent
          <span className="rounded-full bg-raised px-2 py-0.5 text-[11px] font-normal text-secondary">
            预览版
          </span>
        </h1>
      </div>
      <div className="flex items-center gap-2 text-[13px] text-secondary">
        <button
          type="button"
          title="工作区选择（M1 接入）"
          className="flex items-center gap-1.5 rounded-lg px-2.5 py-1.5 hover:bg-hover hover:text-text"
        >
          <FolderIcon className="h-4 w-4" />
          默认工作区
          <ChevronDownIcon className="h-3.5 w-3.5" />
        </button>
        <button
          type="button"
          title="沙箱策略（M2 接入，当前默认只读）"
          className="flex items-center gap-1.5 rounded-lg px-2.5 py-1.5 hover:bg-hover hover:text-text"
        >
          <ShieldIcon className="h-4 w-4" />
          只读沙箱
          <ChevronDownIcon className="h-3.5 w-3.5" />
        </button>
      </div>
    </div>
  );
}
