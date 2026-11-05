import { useState } from 'react';

/**
 * 空态欢迎语（参考 ZCode `ConversationDraftEmptyState` 的时段划分与 zh-CN 文案）。
 *
 * 更新时机（本实现比 ZCode 更省）：**只在启动 / 新建对话时取一次本地时间**。
 * - ZCode 用 useState 挂载取时，另有一次 `setTimeout` 预约到下一个时段边界自动换档；
 *   这里是零定时器——父级用 `key` 在新建对话时重新挂载本组件，重新取时。
 * - 空态组件本身只在"新建对话草稿态"渲染，故对话流中不占任何开销。
 * - hour 非法值归一，绝不抛错：ChatView 无 error boundary，欢迎语抛错会白屏整个会话。
 *
 * 时段边界与文案来源：docs/01 登记的参考源 ZCode 的 `docs/ui/chat-empty-time-greeting.md`（05/09/12/14/18/23）。
 */

const LATE_NIGHT = '夜深啦，别忘了照顾好自己哦';

export function getGreeting(hour: number): string {
  const h = Number.isFinite(hour) ? ((Math.trunc(hour) % 24) + 24) % 24 : 0;
  if (h >= 5 && h < 9) return '早上好呀，新的一天开始啦';
  if (h >= 9 && h < 12) return '上午好呀，有什么想让我帮忙的吗';
  if (h >= 12 && h < 14) return '中午好呀，要不要先休息一下';
  if (h >= 14 && h < 18) return '下午好呀，接下来交给我吧';
  if (h >= 18 && h < 23) return '晚上好呀，今天辛苦啦';
  return LATE_NIGHT; // 23:00–04:59
}

export function Greeting() {
  // useState 初始化器：挂载时取一次，不设任何定时器/effect；新建对话由父级换 key 重新挂载。
  const [text] = useState(() => getGreeting(new Date().getHours()));
  return (
    <p
      data-testid="greeting"
      className="mb-7 text-center text-[22px] leading-8 font-medium text-primary"
    >
      {text}
    </p>
  );
}
