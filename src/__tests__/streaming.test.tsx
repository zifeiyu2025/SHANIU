import { describe, expect, it } from 'vitest';
import { render } from '@testing-library/react';
import { StreamingText } from '../components/markdown/StreamingText';

// FF-7（docs §5）：已完成的 markdown 块重渲染次数 = 0
describe('StreamingText 冻结块', () => {
  it('追加流式内容时已完成块不重渲染', () => {
    const renders: string[] = [];
    const onBlockRender = (text: string) => renders.push(text);
    const count = (block: string) => renders.filter((t) => t === block).length;

    const view = render(
      <StreamingText text={'第一段'} streaming onBlockRender={onBlockRender} />,
    );
    // 尾部安全边界要留两块（dsh UNSTABLE_TAIL_BLOCKS），所以第三块出现时第一块才定稿。
    view.rerender(
      <StreamingText text={'第一段\n\n第二段\n\n第三段流'} streaming onBlockRender={onBlockRender} />,
    );
    const afterFreeze = count('第一段');
    view.rerender(
      <StreamingText text={'第一段\n\n第二段\n\n第三段流式'} streaming onBlockRender={onBlockRender} />,
    );
    view.rerender(
      <StreamingText text={'第一段\n\n第二段\n\n第三段流式中'} streaming onBlockRender={onBlockRender} />,
    );

    // 定稿之后每帧增量必须为 0：缓存的是 element 引用，React 直接跳过整棵子树。
    expect(count('第一段')).toBe(afterFreeze);
    // 活动块随 delta 更新
    expect(count('第三段流式')).toBeGreaterThanOrEqual(1);
    expect(count('第三段流式中')).toBe(1);
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
