import { act, render, screen } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { getGreeting, Greeting } from '../components/chat/Greeting';

// 时段划分与文案对齐 ZCode docs/ui/chat-empty-time-greeting.md（05/09/12/14/18/23）
describe('getGreeting（ZCode 六时段）', () => {
  it('六个时段的边界小时各归其位（含跨 0 点的深夜、截图原句 12 点）', () => {
    const cases: Array<[number, string]> = [
      [0, '夜深啦，别忘了照顾好自己哦'],
      [4, '夜深啦，别忘了照顾好自己哦'],
      [23, '夜深啦，别忘了照顾好自己哦'],
      [5, '早上好呀，新的一天开始啦'],
      [8, '早上好呀，新的一天开始啦'],
      [9, '上午好呀，有什么想让我帮忙的吗'],
      [11, '上午好呀，有什么想让我帮忙的吗'],
      [12, '中午好呀，要不要先休息一下'],
      [13, '中午好呀，要不要先休息一下'],
      [14, '下午好呀，接下来交给我吧'],
      [17, '下午好呀，接下来交给我吧'],
      [18, '晚上好呀，今天辛苦啦'],
      [22, '晚上好呀，今天辛苦啦'],
    ];
    for (const [hour, expected] of cases) {
      expect(getGreeting(hour), `hour=${hour}`).toBe(expected);
    }
  });

  it('非法小时归一且不抛错（欢迎语不得让会话白屏）', () => {
    // 写死期望，避免"用被测实现验证被测实现"：越界/非有限值都应落夜深档（归一到 0 或 23）
    for (const bad of [-1, 24, Number.NaN, Number.POSITIVE_INFINITY]) {
      expect(getGreeting(bad), `hour=${bad}`).toBe('夜深啦，别忘了照顾好自己哦');
    }
    expect(getGreeting(7.8)).toBe('早上好呀，新的一天开始啦'); // 小数截整到 7 点
  });
});

describe('Greeting 组件（零定时器）', () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  it('挂载时取一次时间；零定时器且时间流逝后不自动换档', () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date('2026-11-05T12:30:00'));
    render(<Greeting />);
    expect(screen.getByTestId('greeting').textContent).toBe('中午好呀，要不要先休息一下');
    // 机械锁定"零定时器"：组件不得注册任何 interval/timeout（比对人工读码更硬）
    expect(vi.getTimerCount()).toBe(0);

    act(() => {
      vi.setSystemTime(new Date('2026-11-05T14:30:00'));
      vi.advanceTimersByTime(2 * 60 * 60 * 1000);
    });
    // 只在启动/新建对话取时：单纯时间流逝不改变文案
    expect(screen.getByTestId('greeting').textContent).toBe('中午好呀，要不要先休息一下');
    expect(vi.getTimerCount()).toBe(0);
  });
});
