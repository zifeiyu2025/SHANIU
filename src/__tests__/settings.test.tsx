import { describe, expect, it, vi } from 'vitest';
import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { SettingsDialog } from '../components/settings/SettingsDialog';
import type { Transport } from '../transport/types';

function fakeTransport(overrides?: Partial<Transport>) {
  const saveSettings = vi.fn().mockResolvedValue(undefined);
  const getSettings = vi.fn().mockResolvedValue({
    baseUrl: 'https://api.old.com/v1',
    model: 'm-old',
    workspaceRoot: 'D:/old',
    hasApiKey: true,
  });
  const transport: Transport = {
    ping: vi.fn().mockResolvedValue('n'),
    sendTask: vi.fn().mockResolvedValue(undefined),
    cancel: vi.fn().mockResolvedValue(undefined),
    getSettings,
    saveSettings,
    approve: vi.fn().mockResolvedValue(undefined),
    onEvent: vi.fn().mockReturnValue(() => {}),
    ...overrides,
  };
  return { transport, saveSettings, getSettings };
}

describe('SettingsDialog', () => {
  it('加载已保存设置并渲染字段与 Key 状态', async () => {
    const { transport } = fakeTransport();
    render(<SettingsDialog transport={transport} onClose={() => {}} />);
    await waitFor(() => expect(screen.getByTestId('settings-base-url')).toHaveProperty('value', 'https://api.old.com/v1'));
    expect(screen.getByTestId('settings-model')).toHaveProperty('value', 'm-old');
    expect(screen.getByTestId('settings-workspace')).toHaveProperty('value', 'D:/old');
    expect(screen.getByText(/已保存，留空不覆盖/)).toBeTruthy();
    expect(screen.getByTestId('settings-api-key')).toHaveProperty('value', '');
  });

  it('保存时携带全部字段；Key 留空传 undefined（不覆盖）', async () => {
    const { transport, saveSettings } = fakeTransport();
    const onClose = vi.fn();
    render(<SettingsDialog transport={transport} onClose={onClose} />);
    await waitFor(() => expect(screen.getByTestId('settings-base-url')).toHaveProperty('value', 'https://api.old.com/v1'));

    fireEvent.change(screen.getByTestId('settings-base-url'), { target: { value: 'https://new/v1/' } });
    fireEvent.change(screen.getByTestId('settings-model'), { target: { value: 'm-new' } });
    fireEvent.click(screen.getByRole('button', { name: '保存' }));

    await waitFor(() => expect(saveSettings).toHaveBeenCalledTimes(1));
    expect(saveSettings).toHaveBeenCalledWith({
      baseUrl: 'https://new/v1/', // 前端原样，尾斜杠由后端清洗
      model: 'm-new',
      workspaceRoot: 'D:/old',
      apiKey: undefined,
    });
    await waitFor(() => expect(onClose).toHaveBeenCalledTimes(1));
  });

  it('填写 Key 时随设置一并提交', async () => {
    const { transport, saveSettings } = fakeTransport();
    render(<SettingsDialog transport={transport} onClose={() => {}} />);
    await waitFor(() => expect((screen.getByTestId("settings-api-key") as HTMLInputElement).disabled).toBe(false));
    fireEvent.change(screen.getByTestId('settings-api-key'), { target: { value: 'sk-live' } });
    fireEvent.click(screen.getByRole('button', { name: '保存' }));
    await waitFor(() => expect(saveSettings).toHaveBeenCalledWith(expect.objectContaining({ apiKey: 'sk-live' })));
  });

  it('保存失败时展示错误且不关闭', async () => {
    const { transport } = fakeTransport({
      saveSettings: vi.fn().mockRejectedValue(new Error('磁盘只读')),
    });
    const onClose = vi.fn();
    render(<SettingsDialog transport={transport} onClose={onClose} />);
    await waitFor(() => expect((screen.getByTestId("settings-model") as HTMLInputElement).disabled).toBe(false));
    fireEvent.click(screen.getByRole('button', { name: '保存' }));
    await waitFor(() => expect(screen.getByRole('alert')).toBeTruthy());
    expect(onClose).not.toHaveBeenCalled();
  });

  it('外观三档：名字不被 label 污染，点击即落属性并持久化', async () => {
    const { transport } = fakeTransport();
    render(<SettingsDialog transport={transport} onClose={() => {}} />);
    await waitFor(() => expect(screen.getByTestId('settings-theme')).toBeTruthy());

    // 每个 radio 的可访问名必须就是自己的档位文案；被外层 label 污染时这里会红。
    expect(screen.getAllByRole('radio').map((el) => el.textContent)).toEqual(['浅色', '深色', '跟随系统']);
    expect(screen.getByRole('radio', { name: '浅色' })).toBeTruthy();

    fireEvent.click(screen.getByRole('radio', { name: '深色' }));
    expect(document.body.hasAttribute('data-ds-dark-theme')).toBe(true);
    expect(window.localStorage.getItem('shaniu.theme')).toBe('dark');
    expect(screen.getByRole('radio', { name: '深色' }).getAttribute('aria-checked')).toBe('true');

    fireEvent.click(screen.getByRole('radio', { name: '浅色' }));
    expect(document.body.hasAttribute('data-ds-dark-theme')).toBe(false);
    expect(window.localStorage.getItem('shaniu.theme')).toBe('light');

    document.body.removeAttribute('data-ds-dark-theme');
    window.localStorage.removeItem('shaniu.theme');
  });
});
