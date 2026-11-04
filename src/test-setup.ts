// vitest 全局 setup：启用 React Testing Library 的自动清理，
// 避免跨用例 DOM 泄漏（vitest 未开 globals 时 RTL 不会自动注册 afterEach）。
import { cleanup } from '@testing-library/react';
import { afterEach } from 'vitest';

afterEach(() => {
  cleanup();
});
