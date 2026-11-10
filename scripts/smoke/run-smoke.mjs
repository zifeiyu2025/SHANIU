// 产物级冒烟：驱动 release 主程序，只验证最贵的四件事。
// 与 scratch 探针同一套连法（WebView2 远程调试 + CDP），不另起一套驱动。
//
// 用法：
//   node scripts/smoke/run-smoke.mjs --exe <release 主程序> [--out <证据目录>]
//        [--port 9333] [--stub-port 18180] [--timeout 480]
// 退出码：0 = 四项全过；1 = 有项失败；2 = 环境起不来（不是产品问题，但同样判红）。
import { spawn } from 'node:child_process'
import { appendFileSync, existsSync, mkdirSync, writeFileSync } from 'node:fs'
import path from 'node:path'
import WS from 'ws'
import { FIXTURE_PDF, FIXTURE_PNG, writeFixtures } from './make-fixtures.mjs'
import { startStub } from './stub-model.mjs'

const argv = process.argv.slice(2)
const arg = (name, fallback) => {
  const i = argv.indexOf(`--${name}`)
  return i >= 0 && i + 1 < argv.length ? argv[i + 1] : fallback
}

const exe = path.resolve(arg('exe', 'src-tauri/target/release/shaniu.exe'))
const outDir = path.resolve(arg('out', 'smoke-out'))
const cdpPort = Number(arg('port', '9333'))
const stubPort = Number(arg('stub-port', '18180'))
const overallTimeout = Number(arg('timeout', '480')) * 1000

if (!existsSync(exe)) {
  console.error(`[smoke] 找不到主程序：${exe}`)
  process.exit(2)
}

const dataDir = path.join(outDir, 'data')
const workspaceDir = path.join(dataDir, 'Workspace')
mkdirSync(workspaceDir, { recursive: true })
writeFixtures(workspaceDir)

const nonce = `N${Date.now().toString(36).toUpperCase()}`
const stub = await startStub({ port: stubPort, nonce })

// 配置里只放本地 stub：冒烟不触外网、不需要真实密钥
writeFileSync(
  path.join(dataDir, 'config.toml'),
  [
    'active_provider = "smoke"',
    '',
    '[[providers]]',
    'id = "smoke"',
    'label = "Smoke"',
    'provider = "openai-chat"',
    `base_url = "http://127.0.0.1:${stubPort}/v1"`,
    'model = "smoke-1"',
    'secret_ref = "SMOKE_KEY"',
    '',
  ].join('\n'),
)

const child = spawn(exe, [], {
  env: {
    ...process.env,
    // 调试端口由宿主在窗口构建时挂上（只认这个环境变量）。
    // 不用 WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS：CI runner 上实测会被丢掉。
    SHANIU_CDP_PORT: String(cdpPort),
    SHANIU_DATA_DIR: dataDir,
  },
  stdio: ['ignore', 'pipe', 'pipe'],
  windowsHide: true,
})

// 主程序 stderr 落证据目录：CI 上「端口没开」这类失败，答案通常就在里面
for (const stream of [child.stdout, child.stderr]) {
  stream?.on('data', (chunk) => appendFileSync(path.join(outDir, 'app.log'), chunk))
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms))

async function waitForTarget(timeoutMs) {
  const deadline = Date.now() + timeoutMs
  let last = null
  while (Date.now() < deadline) {
    try {
      const list = await (await fetch(`http://127.0.0.1:${cdpPort}/json/list`)).json()
      last = list
      const page = list.find((t) => t.type === 'page' && typeof t.webSocketDebuggerUrl === 'string')
      if (page) return page
    } catch (error) {
      last = String(error)
    }
    await sleep(500)
  }
  throw new Error(`CDP 目标未在 ${timeoutMs}ms 内出现：${JSON.stringify(last)}`)
}

let sock = null
let nextId = 0
const pending = new Map()
const send = (method, params = {}) =>
  new Promise((resolve, reject) => {
    const id = ++nextId
    pending.set(id, (msg) => (msg.error ? reject(new Error(`${method}: ${JSON.stringify(msg.error)}`)) : resolve(msg.result)))
    sock.send(JSON.stringify({ id, method, params }))
  })
const evalJs = async (expression) => {
  const r = await send('Runtime.evaluate', { expression, returnByValue: true, awaitPromise: true })
  if (r.exceptionDetails) throw new Error(`页面脚本异常：${r.exceptionDetails.text}`)
  return r.result?.value
}
const shot = async (tag) => {
  try {
    const r = await send('Page.captureScreenshot', { format: 'png' })
    writeFileSync(path.join(outDir, `shot-${tag}.png`), Buffer.from(r.data, 'base64'))
  } catch {
    // 截图失败不影响判定，证据目录里缺一张而已
  }
}
const waitFor = async (expression, timeoutMs, label) => {
  const deadline = Date.now() + timeoutMs
  while (Date.now() < deadline) {
    const value = await evalJs(expression).catch(() => null)
    if (value) return value
    await sleep(500)
  }
  throw new Error(`等待超时 ${timeoutMs}ms：${label}`)
}

/** 打开工作区文件页：优先点右栏的「文件」标签，退回 Ctrl+P 快捷键 */
const openFilesPage = async () => {
  if (await evalJs(`!!document.querySelector('[data-files-page]')`)) return
  const byTab = await evalJs(`(() => {
    const tab = [...document.querySelectorAll('[role="tab"]')].find((n) => (n.innerText || '').trim() === '文件')
    if (!tab) return false
    tab.click()
    return true
  })()`)
  if (!byTab) {
    await evalJs(`window.dispatchEvent(new KeyboardEvent('keydown', {
      key: 'p', code: 'KeyP', ctrlKey: true, bubbles: true, cancelable: true, composed: true,
    }))`)
  }
  await waitFor(`!!document.querySelector('[data-files-page]')`, 30_000, '文件页')
}

/** 在文件树里逐层展开直到目标文件出现并点开；返回点开的路径 */
const openTreeEntry = async (target) => {
  await waitFor(`document.querySelectorAll('[data-files-entry]').length > 0`, 15_000, '文件树条目').catch(
    async () => {
      const text = await evalJs(`(document.body.innerText || '').replace(/\\s+/g, ' ').slice(0, 400)`)
      throw new Error(`文件树没有条目（页面文本：${text}）`)
    },
  )
  for (let attempt = 0; attempt < 8; attempt += 1) {
    const step = await evalJs(`(() => {
      const norm = (s) => (s || '').replaceAll('\\\\', '/')
      const entries = [...document.querySelectorAll('[data-files-entry]')]
      const info = entries.map((n) => ({
        kind: n.getAttribute('data-files-entry'),
        path: norm(n.getAttribute('data-files-path')),
        expanded: n.getAttribute('aria-expanded'),
      }))
      const wanted = ${JSON.stringify(target)}
      const file = entries.find((n) =>
        n.getAttribute('data-files-entry') === 'file' && norm(n.getAttribute('data-files-path')).endsWith(wanted))
      if (file) {
        ;(file.querySelector('button') || file).click()
        return { clicked: 'file', path: norm(file.getAttribute('data-files-path')), entries: info }
      }
      const dir = entries.find((n) =>
        n.getAttribute('data-files-entry') === 'directory' && n.getAttribute('aria-expanded') !== 'true')
      if (!dir) return { clicked: null, entries: info }
      ;(dir.querySelector('button') || dir).click()
      return { clicked: 'dir', path: norm(dir.getAttribute('data-files-path')), entries: info }
    })()`)
    if (step.clicked === 'file') {
      await sleep(800)
      return step.path
    }
    if (step.clicked === null) {
      throw new Error(`文件树里展不开也找不到 ${target}：${JSON.stringify(step.entries)}`)
    }
    await sleep(1200)
  }
  throw new Error(`文件树展开 8 层仍未找到 ${target}`)
}

const checks = []
const record = (name, ok, detail) => {
  checks.push({ name, ok, detail })
  console.log(`[smoke] ${ok ? 'PASS' : 'FAIL'} ${name} ${detail ?? ''}`)
}

const cleanup = async () => {
  try {
    spawn('taskkill', ['/PID', String(child.pid), '/T', '/F'], { stdio: 'ignore' }).unref()
  } catch {
    // 进程可能已自己退出
  }
  await stub.close().catch(() => {})
}

const watchdog = setTimeout(async () => {
  console.error(`[smoke] 总超时 ${overallTimeout}ms，强制收尾`)
  await cleanup()
  process.exit(1)
}, overallTimeout)

let exitCode = 1
try {
  const page = await waitForTarget(90_000)
  sock = new WS(page.webSocketDebuggerUrl, { perMessageDeflate: false })
  await new Promise((resolve, reject) => {
    sock.once('open', resolve)
    sock.once('error', reject)
  })
  sock.on('message', (raw) => {
    const msg = JSON.parse(raw.toString())
    if (msg.id !== undefined && pending.has(msg.id)) {
      pending.get(msg.id)(msg)
      pending.delete(msg.id)
    }
  })
  await send('Page.enable')
  await send('Runtime.enable')

  // 1) 首屏：根节点有内容、启动遮罩已撤
  await waitFor(
    `(() => !!document.getElementById('root')?.childElementCount && !document.getElementById('boot-splash'))()`,
    90_000,
    '首屏渲染',
  )
  const title = await evalJs('document.title')
  const rootChildren = await evalJs("document.getElementById('root')?.childElementCount ?? 0")
  await shot('01-boot')
  record('首屏渲染', rootChildren > 0, `title=${title} rootChildren=${rootChildren}`)

  // 把 stub 的假密钥交给应用自己的命令落盘（走真实 DPAPI 路径，不绕过权限层）
  const keyStored = await evalJs(
    `(async () => { try { await window.__TAURI_INTERNALS__.invoke('store_api_key', { provider: 'SMOKE_KEY', key: 'smoke-key' }); return true } catch (error) { return String(error) } })()`,
  )
  if (keyStored !== true) throw new Error(`store_api_key 失败：${keyStored}`)

  const findEditor = `(() => {
    const el = document.querySelector('[contenteditable="true"]') || document.querySelector('textarea')
    if (!el) return null
    el.focus()
    return el.tagName
  })()`
  let editor = await evalJs(findEditor)
  if (!editor) {
    await evalJs(`(() => {
      const want = ['新建会话', '新会话', '开始新会话', 'New session', '开始']
      const btn = [...document.querySelectorAll('button,[role="button"]')]
        .find((b) => want.some((w) => (b.innerText || '').includes(w)))
      if (btn) btn.click()
      return !!btn
    })()`)
    await sleep(1500)
    editor = await evalJs(findEditor)
  }
  if (!editor) throw new Error('找不到输入框，无法发消息')

  const typeAndSend = async (text) => {
    await evalJs(findEditor)
    await send('Input.insertText', { text })
    await sleep(400)
    const base = { key: 'Enter', code: 'Enter', windowsVirtualKeyCode: 13, nativeVirtualKeyCode: 13 }
    await send('Input.dispatchKeyEvent', { type: 'rawKeyDown', ...base })
    await send('Input.dispatchKeyEvent', { type: 'char', text: '\r', key: 'Enter' })
    await send('Input.dispatchKeyEvent', { type: 'keyUp', ...base })
  }

  // 2) 发一条消息收到回复（走本地 stub，不触外网）
  await typeAndSend('冒烟：请回复一句')
  await waitFor(`(document.body.innerText || '').includes(${JSON.stringify(nonce)})`, 90_000, '助手回复')
  const replyText = await evalJs(
    `(document.body.innerText || '').replace(/\\s+/g, ' ').slice(-200)`,
  )
  await shot('02-reply')
  record('消息收发', true, `nonce=${nonce} tail=${replyText}`)

  // 3) /api/file 出图：文档预览里的相对图必须被宿主媒体路由接住
  //    （聊天的用户消息按纯文本渲染，`![]()` 不会解析成图，所以走文档预览这条真实路径）
  await openFilesPage()
  const openedMd = await openTreeEntry('note.md')
  const imgState = await waitFor(
    `(() => {
      const img = [...document.images].find((i) => (i.src || '').includes('/api/file'))
      if (!img || !img.complete) return null
      return { src: img.src, w: img.naturalWidth, h: img.naturalHeight }
    })()`,
    60_000,
    '/api/file 出图',
  )
  await shot('03-api-file')
  record(
    'api/file 出图',
    imgState.w === FIXTURE_PNG.width && imgState.h === FIXTURE_PNG.height,
    `w=${imgState.w} h=${imgState.h} src=${imgState.src.slice(0, 120)} opened=${openedMd}`,
  )

  // 4) PDF 出页：打开工作区里的单页 PDF，必须有 canvas 且没有报错文案
  await openFilesPage()
  const openedPdf = await openTreeEntry(FIXTURE_PDF.name)
  const pdfState = await waitFor(
    `(() => {
      const canvas = [...document.querySelectorAll('canvas')].find((c) => c.width > 0 && c.height > 0)
      if (!canvas) return null
      return { w: canvas.width, h: canvas.height, text: (document.body.innerText || '').replace(/\\s+/g, ' ').slice(-200) }
    })()`,
    90_000,
    'PDF 出页',
  )
  await shot('04-pdf')
  const pdfBroken = /无法显示|无法打开|加载失败|不能预览/.test(pdfState.text)
  record('PDF 出页', pdfState.w > 0 && pdfState.h > 0 && !pdfBroken, `w=${pdfState.w} h=${pdfState.h} broken=${pdfBroken} opened=${openedPdf}`)

  exitCode = checks.every((c) => c.ok) ? 0 : 1
} catch (error) {
  record('执行', false, String(error?.message ?? error))
  await shot('99-failure')
  exitCode = 1
} finally {
  clearTimeout(watchdog)
  writeFileSync(path.join(outDir, 'summary.json'), JSON.stringify({ exe, nonce, checks }, null, 2))
  await cleanup()
  await sleep(500)
  process.exit(exitCode)
}
