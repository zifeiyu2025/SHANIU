// vitest 全局 setup：启用 React Testing Library 的自动清理，
// 避免跨用例 DOM 泄漏（vitest 未开 globals 时 RTL 不会自动注册 afterEach）。
import { cleanup } from '@testing-library/react';
import { afterEach, beforeAll } from 'vitest';

beforeAll(() => {
  // jsdom 不实现滚动 API：会话区的"流式跟随到底"在测试环境里必须存在但无事可做，
  // 真实滚动行为由浏览器实测（截图验收）覆盖，不在这里假装断言。
  Element.prototype.scrollTo = function scrollTo() {};
});

afterEach(() => {
  cleanup();
});
