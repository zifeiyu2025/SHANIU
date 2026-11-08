/**
 * 映射表守门测试（SH-072）。
 *
 * 「映射在、代码不在」曾经是常态：上游 paths 整份拷贝过来，裁剪包时没同步修剪，
 * 于是 164 条 paths 指向不存在的源码也没人发现。这里把两条不变量钉住：
 *
 * 1. `src/dsh/aliases.ts` 与 `src/dsh/tsconfig.base.json` 的每条映射都指向真实存在的内容；
 * 2. `landlock-run` 是唯一有意为之的例外——别名指向防护桩、类型面仍指向真实声明。
 *
 * 任何人再想「先加映射、回头再放代码」都会在这里失败。
 */
import { existsSync, readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { describe, expect, it } from 'vitest'
import { dshAliases } from './dsh/aliases'
import { grantArgs, launcherPath, probe } from './dsh/shims/landlock-run'

/** 本文件在 `src/` 下，上一级就是仓库根。 */
const root = resolve(import.meta.dirname, '..')

function stripWildcard(target: string): string {
  return target.replace(/\/\*$/, '').replace(/\*.*$/, '')
}

/**
 * JSONC 去注释必须字符串感知：paths 的键里带星号通配（如 ui-式子路径键），
 * 用正则去块注释会把键里的「斜杠 + 星号」当成注释起点，吃掉整段配置。
 * 这里只剥字符串之外的两种注释。
 */
function stripJsonc(text: string): string {
  let out = ''
  let inString = false
  let escape = false
  for (let i = 0; i < text.length; i++) {
    const ch = text[i]
    const next = text[i + 1]
    if (inString) {
      out += ch
      if (escape) escape = false
      else if (ch === '\\') escape = true
      else if (ch === '"') inString = false
      continue
    }
    if (ch === '"') { inString = true; out += ch; continue }
    if (ch === '/' && next === '/') { while (i < text.length && text[i] !== '\n') i++; out += '\n'; continue }
    if (ch === '/' && next === '*') { i += 2; while (i < text.length && !(text[i] === '*' && text[i + 1] === '/')) i++; i++; continue }
    out += ch
  }
  return out.replace(/,(\s*[}\]])/g, '$1')
}

describe('DSH 映射表', () => {
  it('别名表只指向真实存在的源码', () => {
    const missing = dshAliases
      .map(({ replacement }) => replacement.replace(/\$1$/, ''))
      .filter(target => !existsSync(resolve(root, target.replace(/^\//, ''))))
    expect(missing).toEqual([])
  })

  it('tsconfig.base.json 的 paths 只指向真实存在的内容', () => {
    const text = readFileSync(resolve(root, 'src/dsh/tsconfig.base.json'), 'utf8')
    const paths = JSON.parse(stripJsonc(text)).compilerOptions.paths as Record<string, string[]>
    const baseDir = resolve(root, 'src/dsh')
    const missing = Object.entries(paths)
      .filter(([, values]) => !values.some(value => existsSync(resolve(baseDir, stripWildcard(value)))))
      .map(([key]) => key)
    expect(missing).toEqual([])
  })

  it('landlock-run 由显式防护桩接住', () => {
    const guarded = dshAliases
      .filter(({ find }) => find.test('@deepseek-ai/node-addon-system/landlock-run'))
      .map(({ replacement }) => replacement)
    expect(guarded).toEqual(['/src/dsh/shims/landlock-run.ts'])
  })

  it('防护桩一被调用就给出说得清的错误', () => {
    expect(() => launcherPath()).toThrow(/landlock-run 不可用/)
    expect(() => grantArgs({ readOnly: ['/'] })).toThrow(/Landlock 是 Linux 专有 LSM/)
    expect(() => probe()).toThrow(/docs\/06-DSH裁剪决策记录\.md/)
  })

  it('landlock-run 的类型面仍指向真实声明（有意与别名不一致）', () => {
    const text = readFileSync(resolve(root, 'src/dsh/tsconfig.base.json'), 'utf8')
    const paths = JSON.parse(stripJsonc(text)).compilerOptions.paths as Record<string, string[]>
    expect(paths['@deepseek-ai/node-addon-system/landlock-run'])
      .toEqual(['./native/system/packages/entry/src/index.ts'])
  })
})
