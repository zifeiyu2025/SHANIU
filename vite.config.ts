/// <reference types="vitest/config" />
import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import tailwindcss from '@tailwindcss/vite';
import { dshAliases } from './src/dsh/aliases.ts';
import { SHANIU_BRAND_NAME } from './src/brand/brand-name.ts';

export default defineConfig({
  plugins: [react(), tailwindcss()],
  // DSH client 面按打包期替换读取构建期环境量（scripts/types/client-build-environment
  // 声明的最小 process 面）；SHANIU 自有品牌值，其余保持 undefined 语义
  define: {
    'process.env': JSON.stringify({
      NODE_ENV: process.env.NODE_ENV ?? 'development',
      DSH_CLIENT_TITLE: SHANIU_BRAND_NAME,
      DSH_CLIENT_COMMIT_HASH: '',
    }),
  },
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
  build: {
    target: 'es2022',
    outDir: 'dist',
    // 字体一律产出文件，不内联成 data: URI：KaTeX 的 woff2 里有小于默认阈值（4KB）的
    // （Size3），内联后打包产物需要 font-src 放行 data: 才显示公式。H2 收紧 CSP 时
    // 用这条换掉一处放宽，代价是多几个几十 KB 的静态文件。
    assetsInlineLimit: (filePath: string) =>
      /\.(?:woff2?|ttf|otf|eot)$/i.test(filePath) ? false : undefined,
  },
  test: {
    environment: 'jsdom',
    setupFiles: ['./src/test-setup.ts'],
    // vendored DSH 树自带上游测试（tests/*.spec.ts，依赖上游 host 运行时），
    // 不属于 SHANIU 验收面；SH-005~008 的 DSH 测试按批次单独甄别引入。
    exclude: ['**/node_modules/**', 'src/dsh/**'],
    // 零测试文件必须报错：测试文件被误改名或删除时，`pnpm test` 会用 0 个用例退 0，
    // 门禁静默变绿（AGENTS §4.2「响度只许增，不许降」）。测试面已随 SH-007/008 建齐，
    // 不存在「迁移期没有测试」的假红。
    passWithNoTests: false,
    // mock 传输测试是时序型的（刻意延迟模拟流式），GHA runner 比本机慢约 2 倍
    // （本地 2841ms 的用例在 CI 上超默认 5000ms）——放宽而非压缩 mock 延迟，保真度优先。
    testTimeout: 15000,
  },
});
