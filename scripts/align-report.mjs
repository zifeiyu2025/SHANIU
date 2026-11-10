#!/usr/bin/env node
/**
 * 上游对齐账本生成器（只读上游，只写 align/）。
 *
 * 判定方式是逐文件**内容哈希**比对，不比"看起来差不多"：
 *   same            两边都有，sha256 相同
 *   drift           两边都有，sha256 不同
 *   missing         上游有、我们没有
 *   not-applicable  我们有、上游没有（本地新增），或 overrides 显式豁免
 *
 * `reason` / `guard` 一律来自 align/overrides.json（人工维护），脚本不臆造；
 * 没有人工记录的不一致条目打上 `review: needs-review` 待办标记。
 *
 * 为什么 `needs-review` 不占用 `status` 字段：它是"有没有人工记录"的问题，
 * 不是"文件本身是哪一类"的问题。两者混进同一个字段，drift / missing 计数就
 * 永远是 0——而这两类补法完全不同（drift 是合并上游，missing 是决定要不要
 * vendor），混了账本就失去意义。所以 status 只放机械判定，review 单独一列。
 *
 * 输出必须可静态重跑：条目按路径排序、JSON 无时间戳、无绝对路径，
 * 同一份上游 + 同一份本地代码两次运行应逐字节相同。
 */
import { createHash } from 'node:crypto'
import { mkdir, readdir, readFile, writeFile } from 'node:fs/promises'
import { existsSync } from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..')
const upstreamRoot = process.env.DSH_UPSTREAM ?? 'D:\\AI\\deepseek-harness'
const SCOPE = 'packages/client'
const upstreamBase = path.join(upstreamRoot, SCOPE)
const ourBase = path.join(repoRoot, 'src', 'dsh', SCOPE)
const alignDir = path.join(repoRoot, 'align')
const overridesPath = path.join(alignDir, 'overrides.json')

/** 目录级忽略：构建产物与依赖不进账本。 */
const IGNORE_DIRS = new Set(['node_modules', '.git', 'dist', 'build', 'coverage', '.turbo', '.vite', '.cache'])

/**
 * 递归收集目录下所有文件的相对路径（POSIX 分隔符），已排序。
 * @param {string} root 绝对路径
 * @param {string} rel 当前相对路径
 * @returns {Promise<string[]>}
 */
async function walk(root, rel = '') {
  const dir = rel === '' ? root : path.join(root, rel)
  if (!existsSync(dir)) return []
  /** @type {string[]} */
  const out = []
  for (const entry of await readdir(dir, { withFileTypes: true })) {
    const childRel = rel === '' ? entry.name : `${rel}/${entry.name}`
    if (entry.isDirectory()) {
      if (IGNORE_DIRS.has(entry.name)) continue
      out.push(...await walk(root, childRel))
      continue
    }
    if (!entry.isFile()) continue
    out.push(childRel)
  }
  return out
}

/**
 * @param {string} file 绝对路径
 * @returns {Promise<string>} 小写十六进制 sha256
 *
 * 先做行尾归一（CRLF → LF）再哈希：两个仓库都检在 Windows 上，`core.autocrlf`
 * 与编辑器保存策略不一致会让同一份内容一个 CRLF 一个 LF。原始字节哈希会把
 * 这种"检出差异"报成 drift，而它与语义无关、也没法通过合并上游消除。
 * 归一后哈希只反映内容差异；行尾差异本身不再单独计入账本。
 */
async function hashFile(file) {
  const bytes = await readFile(file)
  const text = bytes.toString('latin1').replaceAll('\r\n', '\n')
  return createHash('sha256').update(Buffer.from(text, 'latin1')).digest('hex')
}

/**
 * 读 overrides.json；文件缺失时按"一条记录都没有"处理，不创建、不写。
 * @returns {Promise<Record<string, {reason?: string, guard?: string, status?: string}>>}
 */
async function readOverrides() {
  if (!existsSync(overridesPath)) return {}
  const raw = await readFile(overridesPath, 'utf8')
  /** @type {{entries?: Record<string, unknown>}} */
  const parsed = JSON.parse(raw)
  /** @type {Record<string, {reason?: string, guard?: string, status?: string}>} */
  const map = {}
  for (const [key, value] of Object.entries(parsed.entries ?? {})) {
    map[key.replaceAll('\\', '/').replace(/\/+$/, '/')] = /** @type {any} */ (value)
  }
  return map
}

/**
 * 精确路径优先；否则找最长的目录前缀（以 `/` 结尾）。
 * @param {Record<string, {reason?: string, guard?: string, status?: string}>} overrides
 * @param {string} rel
 */
function lookupOverride(overrides, rel) {
  if (overrides[rel] !== undefined) return { key: rel, value: overrides[rel] }
  const parts = rel.split('/')
  for (let i = parts.length - 1; i > 0; i -= 1) {
    const key = `${parts.slice(0, i).join('/')}/`
    if (overrides[key] !== undefined) return { key, value: overrides[key] }
  }
  return undefined
}

/** 上游其它 packages 的规模（本批不逐条判定）。 */
async function countOtherPackages() {
  const packagesDir = path.join(upstreamRoot, 'packages')
  if (!existsSync(packagesDir)) return []
  /** @type {{group: string, files: number}[]} */
  const rows = []
  for (const entry of await readdir(packagesDir, { withFileTypes: true })) {
    if (!entry.isDirectory() || entry.name === 'client') continue
    rows.push({ group: `packages/${entry.name}`, files: (await walk(packagesDir, entry.name)).length })
  }
  return rows.sort((a, b) => a.group.localeCompare(b.group))
}

async function main() {
  if (!existsSync(upstreamBase)) {
    console.error(`[align] 上游目录不存在：${upstreamBase}`)
    process.exit(1)
  }
  const overrides = await readOverrides()
  const upstreamFiles = new Set(await walk(upstreamBase))
  const ourFiles = new Set(await walk(ourBase))
  const all = [...new Set([...upstreamFiles, ...ourFiles])].sort()

  /** @type {any[]} */
  const entries = []
  for (const rel of all) {
    const up = upstreamFiles.has(rel)
    const us = ourFiles.has(rel)
    const upstreamHash = up ? await hashFile(path.join(upstreamBase, rel)) : null
    const ourHash = us ? await hashFile(path.join(ourBase, rel)) : null
    let status = up && us ? (upstreamHash === ourHash ? 'same' : 'drift')
      : up ? 'missing'
        : 'not-applicable'

    const hit = lookupOverride(overrides, rel)
    const forced = hit?.value.status
    if (forced !== undefined) status = forced
    const reason = hit?.value.reason ?? null
    const guard = hit?.value.guard ?? null
    // 待办标记：same 不需要人工记录；其余状态没有 overrides 条目就逼人补。
    const review = status === 'same' || hit !== undefined ? 'recorded' : 'needs-review'

    entries.push({
      upstreamPath: up ? `${SCOPE}/${rel}` : null,
      ourPath: us ? `src/dsh/${SCOPE}/${rel}` : null,
      status,
      review,
      reason,
      guard,
      upstreamHash,
      ourHash,
    })
  }

  const stats = {
    total: entries.length,
    same: 0,
    drift: 0,
    missing: 0,
    'not-applicable': 0,
    'needs-review': 0,
  }
  for (const entry of entries) {
    stats[entry.status] += 1
    if (entry.review === 'needs-review') stats['needs-review'] += 1
  }

  await mkdir(alignDir, { recursive: true })
  await writeFile(path.join(alignDir, 'ledger.json'), `${JSON.stringify({
    version: 1,
    scope: { upstream: SCOPE, ours: `src/dsh/${SCOPE}` },
    stats,
    entries,
  }, null, 2)}\n`, 'utf8')

  const line = (entry) =>
    `| \`${entry.upstreamPath ?? entry.ourPath}\` | ${entry.status} | ${entry.review === 'needs-review' ? '**needs-review**' : '已登记'} | ${entry.reason ?? '—'} | ${entry.guard ?? '—'} |`
  const md = [
    '# 上游对齐账本（机器生成）',
    '',
    '> 由 `scripts/align-report.mjs` 生成，请勿手改；人工说明维护在 `align/overrides.json`。',
    '> `status` 四值：`same` / `drift` / `missing` / `not-applicable`（逐文件 sha256 比对）。',
    '> `review` 两值：`已登记` / `needs-review`（后者=还没有人工记录，必须补 overrides）。',
    '',
    '## 统计',
    '',
    `- 条目总数：${stats.total}`,
    `- same：${stats.same}`,
    `- drift：${stats.drift}`,
    `- missing：${stats.missing}`,
    `- not-applicable：${stats['not-applicable']}`,
    `- 待复核（needs-review）：${stats['needs-review']}`,
    '',
    '## 不一致条目',
    '',
    '| 文件 | 状态 | 人工记录 | 说明 | 兜底 |',
    '| --- | --- | --- | --- | --- |',
    ...entries.filter(entry => entry.status !== 'same').map(line),
    '',
    '## 上游其它 packages（本批未覆盖，仅统计规模）',
    '',
    '| 分组 | 文件数 |',
    '| --- | --- |',
    ...(await countOtherPackages()).map(row => `| ${row.group} | ${row.files} |`),
    '',
  ].join('\n')
  await writeFile(path.join(alignDir, 'ledger.md'), md, 'utf8')

  console.log(`[align] total=${stats.total} same=${stats.same} drift=${stats.drift} missing=${stats.missing} n/a=${stats['not-applicable']} needs-review=${stats['needs-review']}`)
  for (const entry of entries.filter(item => item.status === 'drift' || item.status === 'missing')) {
    console.log(`  ${entry.status.padEnd(8)} ${entry.upstreamPath ?? entry.ourPath}`)
  }
}

await main()
