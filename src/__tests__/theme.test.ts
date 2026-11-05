import { afterEach, describe, expect, it, vi } from 'vitest';
import {
  THEME_PREFERENCES,
  applyPreference,
  readPreference,
  resolvesDark,
  writePreference,
} from '../theme';

// SH-041：三档偏好的解析与落属性。机制必须与 dsh 一致——CSS 层只认
// body[data-ds-dark-theme]，判定只发生在 theme.ts 一处。
describe('主题偏好', () => {
  afterEach(() => {
    document.body.removeAttribute('data-ds-dark-theme');
    window.localStorage.removeItem('shaniu.theme');
  });

  it('三档解析：显式档压过系统，system 跟随系统', () => {
    expect(resolvesDark('dark', false)).toBe(true);
    expect(resolvesDark('light', true)).toBe(false);
    expect(resolvesDark('system', true)).toBe(true);
    expect(resolvesDark('system', false)).toBe(false);
  });

  it('applyPreference 只通过 body 属性表达暗色（CSS 层认的就是它）', () => {
    expect(applyPreference('dark', false)).toBe(true);
    expect(document.body.hasAttribute('data-ds-dark-theme')).toBe(true);

    expect(applyPreference('light', true)).toBe(false);
    expect(document.body.hasAttribute('data-ds-dark-theme')).toBe(false);

    expect(applyPreference('system', true)).toBe(true);
    expect(document.body.getAttribute('data-ds-dark-theme')).toBe('');
  });

  it('偏好可持久化，非法值与读不到都回落 system', () => {
    expect(readPreference()).toBe('system'); // 默认档
    writePreference('dark');
    expect(readPreference()).toBe('dark');

    window.localStorage.setItem('shaniu.theme', 'neon');
    expect(readPreference()).toBe('system');

    window.localStorage.removeItem('shaniu.theme');
    expect(readPreference()).toBe('system');
    expect(THEME_PREFERENCES).toEqual(['light', 'dark', 'system']);
  });

  it('storage 被禁用时不抛错：主题不是能让应用白屏的依赖', () => {
    const broken = {
      getItem: () => {
        throw new Error('SecurityError');
      },
      setItem: () => {
        throw new Error('QuotaExceeded');
      },
    };
    const spy = vi.spyOn(window, 'localStorage', 'get').mockReturnValue(broken as unknown as Storage);

    expect(readPreference()).toBe('system');
    expect(() => writePreference('dark')).not.toThrow();
    spy.mockRestore();
  });
});
