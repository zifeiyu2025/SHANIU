#!/usr/bin/env node
// SH-136：构建期占位符的产物门禁。
//
// 单测里的 `vi.stubGlobal('__DSH_PDFJS_ASSETS__', …)` 测不出"Vite 忘了注入"：
// 注入缺失只会在真实构建产物里留下未替换的标识符（运行时报
// `__DSH_PDFJS_ASSETS__ is not defined`）。这条检查直接扫 `dist/assets/*.js`。
import { readdirSync, readFileSync } from 'node:fs'
import { join } from 'node:path'

const assets = join(process.cwd(), 'dist', 'assets')
const files = readdirSync(assets).filter((name) => name.endsWith('.js'))
const sources = files.map((name) => [name, readFileSync(join(assets, name), 'utf8')])

const leaked = sources.filter(([, source]) => source.includes('__DSH_PDFJS_ASSETS__')).map(([name]) => name)
if (leaked.length > 0) {
  console.error(`[check-dist-assets] 占位符残留：${leaked.join(', ')}`)
  process.exit(1)
}

// 反向对照：删掉 define 时占位符会留下；但若有人只把占位符改名，也必须在产物里
// 看到真正内联的资产表（PDF.js 的 BinaryDataFactory 请求按 cMapUrl 等三种 kind 分派）。
const inlined = sources.some(([, source]) => source.includes('"cMapUrl"'))
if (!inlined) {
  console.error('[check-dist-assets] 产物里没有 PDF.js 资产表（cMapUrl）——检查 vite.config.ts 的 define')
  process.exit(1)
}

console.log(`[check-dist-assets] ok：${files.length} 个 JS 产物无占位符残留，PDF.js 资产表已内联`)
