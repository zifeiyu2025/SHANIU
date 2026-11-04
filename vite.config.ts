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
  test: { environment: 'jsdom', setupFiles: ['./src/test-setup.ts'] },
});
