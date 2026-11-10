/// <reference types="vitest/config" />
import { readFileSync, readdirSync } from 'node:fs';
import { createRequire } from 'node:module';
import { dirname, join } from 'node:path';
import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import tailwindcss from '@tailwindcss/vite';
import { dshAliases } from './src/dsh/aliases.ts';
import { SHANIU_BRAND_NAME } from './src/brand/brand-name.ts';

/**
 * PDF.js 的 cMap / 标准字体 / wasm 按上游做法全量内联（SH-136）。
 *
 * 上游由 tsdown 插件注入 `__DSH_PDFJS_ASSETS__`（`ui-sidebar-documentpreview/tsdown.config.ts`），
 * 我们的前端走 Vite，没有这个注入点，运行时便抛 `__DSH_PDFJS_ASSETS__ is not defined`。
 * 这里用同一份目录约定读成 `{kind: {文件名: base64}}`：不在运行时发网络请求，也不引入
 * 额外静态目录（打包后 `dist` 之外没有可服务路径）。
 */
function pdfjsAssets(): string {
  const root = dirname(createRequire(import.meta.url).resolve('pdfjs-dist/package.json'));
  const kinds: ReadonlyArray<readonly [string, string]> = [
    ['cMapUrl', 'cmaps'],
    ['standardFontDataUrl', 'standard_fonts'],
    ['wasmUrl', 'wasm'],
  ];
  return JSON.stringify(Object.fromEntries(kinds.map(([kind, directory]) => [
    kind,
    Object.fromEntries(readdirSync(join(root, directory))
      .filter((name) => !name.startsWith('LICENSE'))
      .sort()
      .map((name) => [name, readFileSync(join(root, directory, name)).toString('base64')])),
  ])));
}


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
    // SH-136：PDF 预览依赖的构建期资产表，见 `pdfjsAssets()` 注释。
    __DSH_PDFJS_ASSETS__: pdfjsAssets(),
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
