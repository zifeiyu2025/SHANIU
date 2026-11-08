/**
 * 主题令牌守卫。
 *
 * 背景：`mcp-trust.css` 曾经引用 `--dsh-text-primary` 这类**真实壳里不存在**的变量，
 * 回退值又是浅色文字，结果暗色主题下时间戳几乎不可读——逻辑测试全绿，真机才发现。
 * 移植上游 UI 时这是系统性风险，人眼看截图只能撞运气，必须机检。
 *
 * 规则：
 * - `--dsw-alias-*`（颜色别名）必须同时存在于浅色与暗色令牌块，否则暗色会继承浅色值；
 * - 其他 `--dsw-*`（静态色阶、圆角、阴影、字号）必须在令牌清单里出现；
 * - 其余 `var(--x)`（组件局部自定义属性、宿主注入变量）必须在我们自己的代码里有定义
 *   （CSS 声明 / 对象键 / `setProperty` / 名字常量）。
 *
 * 上游（`src/dsh`）里引用了但令牌清单没有的变量单独列出：那些是 vendored 代码的既有缺口，
 * 不由本仓库修，但要显式登记，新增一处就失败。
 */
import { readdirSync, readFileSync } from 'node:fs'
import { join, relative } from 'node:path'
import { describe, expect, it } from 'vitest'

const SRC = import.meta.dirname
const VENDOR = join(SRC, 'dsh')
const THEME_DIR = join(VENDOR, 'packages', 'client', 'ui-theme', 'src', 'styles')

const SOURCE_FILE = /\.(css|ts|tsx)$/
const TEST_FILE = /\.(spec|test)\.[^.]+$/

/** 上游既有缺口：vendored 代码引用了、但主题令牌里没有的变量（本次排查结果）。 */
const UPSTREAM_MISSING = new Set([
  '--dsw-alias-bg-elevated',
  '--dsw-alias-bg-layer-4',
  '--dsw-alias-fill-l1',
  '--dsw-alias-fill-l2',
  '--dsw-alias-fill-tertiary',
  '--dsw-alias-label-error',
  '--dsw-alias-label-quaternary',
  '--dsw-alias-separator-primary',
  '--dsw-font',
  '--dsw-font-mono',
  '--dsw-hovercard-bg',
])

interface Block {
  selector: string
  body: string
}

/** 极简 CSS 块解析：只处理本项目令牌文件里的扁平规则。 */
function parseBlocks(css: string): Block[] {
  const blocks: Block[] = []
  let index = 0
  let start = 0
  while (index < css.length) {
    const char = css[index]
    if (char === '{') {
      const selector = css.slice(start, index).trim()
      let depth = 1
      let cursor = index + 1
      while (cursor < css.length && depth > 0) {
        if (css[cursor] === '{') depth += 1
        else if (css[cursor] === '}') depth -= 1
        cursor += 1
      }
      blocks.push({ selector, body: css.slice(index + 1, cursor - 1) })
      index = cursor
      start = cursor
    } else if (char === '}') {
      index += 1
      start = index
    } else {
      index += 1
    }
  }
  return blocks
}

interface Inventory {
  all: Set<string>
  light: Set<string>
  dark: Set<string>
}

/** 从 ui-theme 的全部样式文件里汇总令牌清单。 */
function inventory(): Inventory {
  const all = new Set<string>()
  const light = new Set<string>()
  const dark = new Set<string>()
  for (const name of readdirSync(THEME_DIR)) {
    if (!name.endsWith('.css')) continue
    const css = readFileSync(join(THEME_DIR, name), 'utf8')
    for (const block of parseBlocks(css)) {
      const tokens = [...block.body.matchAll(/(--[a-z0-9-]+)\s*:/gi)].map((match) => match[1])
      tokens.forEach((token) => all.add(token))
      if (block.selector.includes('data-ds-dark-theme')) {
        tokens.forEach((token) => dark.add(token))
      } else if (!block.selector.startsWith('@')) {
        tokens.forEach((token) => light.add(token))
      }
    }
  }
  return { all, light, dark }
}

/** 递归列出目录下的源文件（跳过 node_modules / 构建产物）。 */
function collectFiles(dir: string): string[] {
  const files: string[] = []
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const full = join(dir, entry.name)
    if (entry.isDirectory()) {
      if (entry.name === 'node_modules' || entry.name === 'dist') continue
      files.push(...collectFiles(full))
    } else if (SOURCE_FILE.test(entry.name) && !TEST_FILE.test(entry.name)) {
      files.push(full)
    }
  }
  return files
}

const label = (file: string): string => relative(SRC, file).replaceAll('\\', '/')

interface Reference {
  name: string
  where: string
}

/** 收集 `var(--x)` 引用（`var(--x-${...})` 这类运行时拼接不算，靠前瞻排除）。 */
function references(file: string): Reference[] {
  const text = readFileSync(file, 'utf8')
  return [...text.matchAll(/var\(\s*(--[a-z0-9-]+)(?=[\s,)])/gi)].map((match) => ({
    name: match[1],
    where: `${label(file)}:${text.slice(0, match.index).split('\n').length}`,
  }))
}

/** 收集我们自己代码里声明过的自定义属性（CSS 声明、对象键、setProperty、名字常量）。 */
function declarations(files: string[]): Set<string> {
  const names = new Set<string>()
  for (const file of files) {
    const text = readFileSync(file, 'utf8')
    for (const match of text.matchAll(/(--[a-z0-9-]+)\s*['"`]?\s*:/gi)) names.add(match[1])
    for (const match of text.matchAll(/setProperty\(\s*['"`](--[a-z0-9-]+)/gi)) names.add(match[1])
    for (const match of text.matchAll(/(?:const|let|var)\s+\w+\s*=\s*['"`](--[a-z0-9-]+)['"`]/g))
      names.add(match[1])
  }
  return names
}

describe('主题令牌守卫', () => {
  const tokens = inventory()
  const ours = collectFiles(SRC).filter((file) => !file.startsWith(VENDOR))
  const declared = declarations(ours)

  it('令牌清单能解析出来（防止解析器悄悄失效后测试永远为绿）', () => {
    expect(tokens.all.size).toBeGreaterThan(200)
    expect(tokens.light.has('--dsw-alias-label-primary')).toBe(true)
    expect(tokens.dark.has('--dsw-alias-label-primary')).toBe(true)
  })

  it('我们自己写的 UI 只引用真实存在的主题令牌', () => {
    const problems: string[] = []
    for (const file of ours) {
      for (const reference of references(file)) {
        const { name, where } = reference
        if (name.startsWith('--dsw-alias-')) {
          if (!tokens.light.has(name) || !tokens.dark.has(name)) {
            const scope = [
              tokens.light.has(name) ? '浅色有' : '浅色缺',
              tokens.dark.has(name) ? '暗色有' : '暗色缺',
            ].join(' / ')
            problems.push(`${where}  ${name}（${scope}）`)
          }
        } else if (name.startsWith('--dsw-')) {
          if (!tokens.all.has(name)) problems.push(`${where}  ${name}（令牌清单里没有）`)
        } else if (!declared.has(name)) {
          problems.push(`${where}  ${name}（我们自己的代码里没有定义）`)
        }
      }
    }
    expect(problems.join('\n')).toBe('')
  })

  it('上游 `--dsw-*` 引用与令牌清单的差值只有登记过的既有缺口', () => {
    const unknown = new Set<string>()
    for (const file of collectFiles(VENDOR)) {
      for (const reference of references(file)) {
        if (!reference.name.startsWith('--dsw-')) continue
        if (tokens.all.has(reference.name)) continue
        if (UPSTREAM_MISSING.has(reference.name)) continue
        unknown.add(reference.name)
      }
    }
    expect([...unknown].sort()).toEqual([])
  })
})
