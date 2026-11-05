import { describe, expect, it, vi } from 'vitest';
import { fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { SettingsDialog } from '../components/settings/SettingsDialog';
import type { Transport } from '../transport/types';
import { stubTransport } from './stub-transport';

function fakeTransport(overrides?: Partial<Transport>) {
  const saveSettings = vi.fn().mockResolvedValue(undefined);
  const getSettings = vi.fn().mockResolvedValue({
    baseUrl: 'https://api.old.com/v1',
    model: 'm-old',
    workspaceRoot: 'D:/old',
    hasApiKey: true,
    sandboxMode: 'workspace_write',
    approvalMode: 'ask',
  });
  const transport = stubTransport({ getSettings, saveSettings, ...overrides });
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
    // 沙箱与审批没被碰过，也必须原样带回回显值而不是缺省——缺键在后端是 None（不动），
    // 但界面既然已经知道当前档位，就该把"我看到的"和"我写回的"钉成同一个值。
    expect(saveSettings).toHaveBeenCalledWith({
      baseUrl: 'https://new/v1/', // 前端原样，尾斜杠由后端清洗
      model: 'm-new',
      workspaceRoot: 'D:/old',
      apiKey: undefined,
      sandboxMode: 'workspace_write',
      approvalMode: 'ask',
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

    // 作用域必须收窄到外观组：SH-040 之后界面里还有沙箱与审批两组 radio，
    // 全页 getAllByRole 会把 8 个档位一起抓进来，那条"名字不被污染"的断言就失去意义。
    const themeGroup = () => within(screen.getByTestId('settings-theme'));
    expect(themeGroup().getAllByRole('radio').map((el) => el.textContent)).toEqual([
      '浅色',
      '深色',
      '跟随系统',
    ]);
    expect(themeGroup().getByRole('radio', { name: '浅色' })).toBeTruthy();

    fireEvent.click(themeGroup().getByRole('radio', { name: '深色' }));
    expect(document.body.hasAttribute('data-ds-dark-theme')).toBe(true);
    expect(window.localStorage.getItem('shaniu.theme')).toBe('dark');
    expect(themeGroup().getByRole('radio', { name: '深色' }).getAttribute('aria-checked')).toBe('true');

    fireEvent.click(themeGroup().getByRole('radio', { name: '浅色' }));
    expect(document.body.hasAttribute('data-ds-dark-theme')).toBe(false);
    expect(window.localStorage.getItem('shaniu.theme')).toBe('light');

    document.body.removeAttribute('data-ds-dark-theme');
    window.localStorage.removeItem('shaniu.theme');
  });
});

describe('沙箱与审批模式（SH-040）', () => {
  const payload = {
    baseUrl: 'https://api.old.com/v1',
    model: 'm-old',
    workspaceRoot: 'D:/old',
    hasApiKey: true,
    sandboxMode: 'workspace_write',
    approvalMode: 'ask',
  } as const;

  it('两档按后端回显选中，而不是界面自己猜', async () => {
    fakeTransport();
    render(<SettingsDialog transport={fakeTransport().transport} onClose={() => {}} />);
    await waitFor(() => expect(screen.getByTestId('settings-sandbox')).toBeTruthy());
    const sandbox = within(screen.getByTestId('settings-sandbox'));
    expect(sandbox.getByRole('radio', { name: '工作区可写' }).getAttribute('aria-checked')).toBe('true');
    expect(sandbox.getByRole('radio', { name: '只读' }).getAttribute('aria-checked')).toBe('false');
    expect(
      within(screen.getByTestId('settings-approval'))
        .getByRole('radio', { name: '逐次询问' })
        .getAttribute('aria-checked'),
    ).toBe('true');
  });

  it('改档后保存，两个 snake_case 线名原样下行', async () => {
    const { transport, saveSettings } = fakeTransport();
    render(<SettingsDialog transport={transport} onClose={() => {}} />);
    await waitFor(() => expect(screen.getByTestId('settings-sandbox')).toBeTruthy());

    fireEvent.click(within(screen.getByTestId('settings-sandbox')).getByRole('radio', { name: '只读' }));
    fireEvent.click(
      within(screen.getByTestId('settings-approval')).getByRole('radio', { name: '只读自动放行' }),
    );
    fireEvent.click(screen.getByRole('button', { name: '保存' }));

    await waitFor(() => expect(saveSettings).toHaveBeenCalledTimes(1));
    const input = saveSettings.mock.calls[0][0];
    expect(input.sandboxMode).toBe('read_only');
    expect(input.approvalMode).toBe('auto_read_only');
  });

  it('进入完全访问但未勾选确认时，保存请求根本不发出去', async () => {
    const { transport, saveSettings } = fakeTransport();
    render(<SettingsDialog transport={transport} onClose={() => {}} />);
    await waitFor(() => expect(screen.getByTestId('settings-sandbox')).toBeTruthy());

    fireEvent.click(
      within(screen.getByTestId('settings-sandbox')).getByRole('radio', { name: '完全访问' }),
    );
    // 警告块只在"正在进入"时出现，不是常驻噪音
    expect(screen.getByTestId('danger-confirm')).toBeTruthy();
    expect(screen.getByTestId('settings-sandbox-hint').textContent).toContain('不经审批');

    fireEvent.click(screen.getByRole('button', { name: '保存' }));
    await Promise.resolve();
    expect(saveSettings).not.toHaveBeenCalled();
    expect(screen.getByRole('alert').textContent).toContain('需要先勾选');
  });

  it('勾选确认后同一档位才真的落盘', async () => {
    const { transport, saveSettings } = fakeTransport();
    const onClose = vi.fn();
    render(<SettingsDialog transport={transport} onClose={onClose} />);
    await waitFor(() => expect(screen.getByTestId('settings-sandbox')).toBeTruthy());

    fireEvent.click(
      within(screen.getByTestId('settings-sandbox')).getByRole('radio', { name: '完全访问' }),
    );
    fireEvent.click(screen.getByTestId('danger-ack'));
    fireEvent.click(screen.getByRole('button', { name: '保存' }));

    await waitFor(() => expect(saveSettings).toHaveBeenCalledTimes(1));
    expect(saveSettings.mock.calls[0][0].sandboxMode).toBe('danger_full_access');
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it('配置本来就是完全访问时，改端点不会被确认框挡住', async () => {
    // gate 的判据是"正在进入危险档"，不是"当前处于危险档"——否则用户每次改模型
    // 都要重新确认一次沙箱，那只会训练人盲勾。
    const { transport, saveSettings } = fakeTransport({
      getSettings: vi.fn().mockResolvedValue({ ...payload, sandboxMode: 'danger_full_access' }),
    });
    render(<SettingsDialog transport={transport} onClose={() => {}} />);
    await waitFor(() => expect(screen.getByTestId('settings-sandbox')).toBeTruthy());

    expect(screen.queryByTestId('danger-confirm')).toBeNull();
    fireEvent.click(within(screen.getByTestId('settings-sandbox')).getByRole('radio', { name: '只读' }));
    fireEvent.click(screen.getByRole('button', { name: '保存' }));
    await waitFor(() => expect(saveSettings).toHaveBeenCalledTimes(1));
    expect(saveSettings.mock.calls[0][0].sandboxMode).toBe('read_only');
  });

  it('保存失败时留在卡里并报错，不假装已生效', async () => {
    const onClose = vi.fn();
    const { transport } = fakeTransport({
      saveSettings: vi.fn().mockRejectedValue(new Error('沙箱模式取值非法')),
    });
    render(<SettingsDialog transport={transport} onClose={onClose} />);
    await waitFor(() => expect(screen.getByTestId('settings-sandbox')).toBeTruthy());
    fireEvent.click(screen.getByRole('button', { name: '保存' }));
    await waitFor(() => expect(screen.getByRole('alert').textContent).toContain('沙箱模式取值非法'));
    expect(onClose).not.toHaveBeenCalled();
  });
});
