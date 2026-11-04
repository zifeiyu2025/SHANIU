import { describe, expect, it, vi } from 'vitest';
import { fireEvent, render, screen } from '@testing-library/react';
import { ToolPanel } from '../components/chrome/ToolPanel';

describe('ToolPanel（右侧工具面板）', () => {
  it('渲染「开始」头与四张工具卡片（含快捷键标注）', () => {
    render(<ToolPanel onClose={() => {}} />);
    expect(screen.getByTestId('tool-panel')).toBeTruthy();
    expect(screen.getByText('工作区文件')).toBeTruthy();
    expect(screen.getByText('浏览会话工作区的文件')).toBeTruthy();
    expect(screen.getByText('Ctrl + P')).toBeTruthy();
    expect(screen.getByText('新建终端')).toBeTruthy();
    expect(screen.getByText('浏览器')).toBeTruthy();
    expect(screen.getByText('更多工具')).toBeTruthy();
  });

  it('点击关闭按钮触发 onClose', () => {
    const onClose = vi.fn();
    render(<ToolPanel onClose={onClose} />);
    fireEvent.click(screen.getByRole('button', { name: '关闭工具面板' }));
    expect(onClose).toHaveBeenCalledTimes(1);
  });
});
