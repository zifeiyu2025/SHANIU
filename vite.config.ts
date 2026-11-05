/// <reference types="vitest/config" />
import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import tailwindcss from '@tailwindcss/vite';
import { dshAliases } from './src/dsh/aliases.ts';

export default defineConfig({
  plugins: [react(), tailwindcss()],
  resolve: {
    alias: [
      // 传输面 shim 先于通用别名：typert 生成物（/remote）与聚合包由适配层取代；
      // gateway 本身是通用流机械（不含生成物），用真实源码
      { find: /^@deepseek-ai\/dsh-api-remotes(\/.*)?$/, replacement: '/src/transport/shims/remotes-client.ts' },
      { find: /^@deepseek-ai\/[a-z0-9-]+\/remote$/, replacement: '/src/transport/shims/remote-module.ts' },
      ...dshAliases,
    ],
  },
  // Tauri 约定：不清屏、固定端口，便于 cargo tauri dev 挂接
  clearScreen: false,
  server: { port: 5173, strictPort: true },
  build: { target: 'es2022', outDir: 'dist' },
  test: {
    environment: 'jsdom',
    setupFiles: ['./src/test-setup.ts'],
    // vendored DSH 树自带上游测试（tests/*.spec.ts，依赖上游 host 运行时），
    // 不属于 SHANIU 验收面；SH-005~008 的 DSH 测试按批次单独甄别引入。
    exclude: ['**/node_modules/**', 'src/dsh/**'],
    // 旧自研 UI 及其测试已按 ADR-006 移除；测试面随 DSH 批次接线（SH-007/008）重建，
    // 迁移期允许零测试文件通过，避免假绿。
    passWithNoTests: true,
    // mock 传输测试是时序型的（刻意延迟模拟流式），GHA runner 比本机慢约 2 倍
    // （本地 2841ms 的用例在 CI 上超默认 5000ms）——放宽而非压缩 mock 延迟，保真度优先。
    testTimeout: 15000,
  },
});
