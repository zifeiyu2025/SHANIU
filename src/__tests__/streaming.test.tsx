import { describe, expect, it } from 'vitest';
import { render } from '@testing-library/react';
import { StreamingText } from '../components/markdown/StreamingText';

// FF-7（docs §5）：已完成的 markdown 块重渲染次数 = 0
describe('StreamingText 冻结块', () => {
  it('追加流式内容时已完成块不重渲染', () => {
    const renders: string[] = [];
    const onBlockRender = (text: string) => renders.push(text);

    const view = render(
      <StreamingText text={'第一段'} streaming onBlockRender={onBlockRender} />,
    );
    view.rerender(
      <StreamingText text={'第一段\n\n第二段流式'} streaming onBlockRender={onBlockRender} />,
    );
    view.rerender(
      <StreamingText text={'第一段\n\n第二段流式中'} streaming onBlockRender={onBlockRender} />,
    );

    // 冻结块只在挂载时渲染一次
    expect(renders.filter((t) => t === '第一段')).toHaveLength(1);
    // 活动块随 delta 更新
    expect(renders.filter((t) => t.startsWith('第二段')).length).toBeGreaterThanOrEqual(2);
  });

  it('完成态：末块冻结、光标消失、全文可见', () => {
    const renders: string[] = [];
    const onBlockRender = (text: string) => renders.push(text);
    const view = render(
      <StreamingText text={'第一段\n\n第二段'} streaming onBlockRender={onBlockRender} />,
    );
    view.rerender(
      <StreamingText text={'第一段\n\n第二段'} streaming={false} onBlockRender={onBlockRender} />,
    );
    expect(view.queryByTestId('streaming-cursor')).toBeNull();
    expect(view.getByText('第一段')).toBeTruthy();
    expect(view.getByText('第二段')).toBeTruthy();
    // streaming=false 不改变块文本，冻结块不再重渲染
    expect(renders.filter((t) => t === '第一段')).toHaveLength(1);
  });

  it('空行分段与多余空行过滤', () => {
    const view = render(<StreamingText text={'A\n\n\n\nB'} streaming={false} />);
    expect(view.getByText('A')).toBeTruthy();
    expect(view.getByText('B')).toBeTruthy();
    expect(view.container.querySelectorAll('p')).toHaveLength(2);
  });
});
