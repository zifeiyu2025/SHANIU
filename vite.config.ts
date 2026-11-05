/// <reference types="vitest/config" />
import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import tailwindcss from '@tailwindcss/vite';

export default defineConfig({
  plugins: [react(), tailwindcss()],
  // Tauri 约定：不清屏、固定端口，便于 cargo tauri dev 挂接
  clearScreen: false,
  server: { port: 5173, strictPort: true },
  build: { target: 'es2022', outDir: 'dist' },
  test: {
    environment: 'jsdom',
    setupFiles: ['./src/test-setup.ts'],
    // mock 传输测试是时序型的（刻意延迟模拟流式），GHA runner 比本机慢约 2 倍
    // （本地 2841ms 的用例在 CI 上超默认 5000ms）——放宽而非压缩 mock 延迟，保真度优先。
    testTimeout: 15000,
  },
});
