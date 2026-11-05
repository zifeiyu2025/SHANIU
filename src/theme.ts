// 主题偏好（SH-041）。机制照抄 dsh：ui-theme/src/boot-theme.ts:39-42 用
// `document.body.toggleAttribute('data-ds-dark-theme', dark)`，CSS 层只认这个属性，
// 三档偏好（light/dark/system，同 dsh THEME_PREFERENCES）只在**这一处**解析。
//
// 为什么不用 `.dark` 类、也不在 CSS 里直接写 prefers-color-scheme：
// 那样"跟随系统"就要在两处判定（CSS 媒体查询 + JS 开关），两者对"系统"的理解
// 一旦不一致（比如用户在系统外又选了档），就会出现组件层无法解释的中间态。
//
// 存储位置是 WebView localStorage，而不是 config.toml：后端配置里没有主题字段，
// 加一个属于配置面变更（要同步 04 §4 与设置读写命令）。主题偏好是纯显示状态，
// 落在这里既不越界也不需要动协议；代价是换 WebView 数据目录不跟随，已知可接受。

export type ThemePreference = 'light' | 'dark' | 'system';

const STORAGE_KEY = 'shaniu.theme';
export const THEME_PREFERENCES: readonly ThemePreference[] = ['light', 'dark', 'system'];

export const THEME_LABELS: Record<ThemePreference, string> = {
  light: '浅色',
  dark: '深色',
  system: '跟随系统',
};

function isPreference(value: unknown): value is ThemePreference {
  return typeof value === 'string' && (THEME_PREFERENCES as readonly string[]).includes(value);
}

export function readPreference(): ThemePreference {
  try {
    const raw = window.localStorage.getItem(STORAGE_KEY);
    return isPreference(raw) ? raw : 'system';
  } catch {
    // 隐私模式或 WebView 禁用 storage：退回"跟随系统"，绝不因为读不到偏好而白屏
    return 'system';
  }
}

export function writePreference(preference: ThemePreference): void {
  try {
    window.localStorage.setItem(STORAGE_KEY, preference);
  } catch {
    /* 存不下也要让本次切换生效，下次启动回到默认即可 */
  }
}

/** 纯函数：给定偏好与系统态，是否该上暗色。单独拆出来是为了可测——不碰 DOM 也能断言三档逻辑。 */
export function resolvesDark(preference: ThemePreference, systemDark: boolean): boolean {
  if (preference === 'dark') return true;
  if (preference === 'light') return false;
  return systemDark;
}

/** 把偏好落到 body 属性上，返回当前是否暗色（供调用方同步自身状态）。 */
export function applyPreference(preference: ThemePreference, systemDark = systemIsDark()): boolean {
  const dark = resolvesDark(preference, systemDark);
  document.body.toggleAttribute('data-ds-dark-theme', dark);
  return dark;
}

export function systemIsDark(): boolean {
  return window.matchMedia?.('(prefers-color-scheme: dark)').matches ?? false;
}

/**
 * 启动接线：应用一次当前偏好，并在"跟随系统"时跟随系统切换。
 * 显式选了浅色/深色就不响应系统变化——这是用户的主动选择，系统不该反过来覆盖它。
 */
export function initTheme(): ThemePreference {
  const preference = readPreference();
  applyPreference(preference);
  window
    .matchMedia?.('(prefers-color-scheme: dark)')
    .addEventListener?.('change', (e: MediaQueryListEvent) => {
      if (readPreference() === 'system') applyPreference('system', e.matches);
    });
  return preference;
}
