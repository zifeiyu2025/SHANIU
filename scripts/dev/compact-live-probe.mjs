// 压缩窗口真机复验探针（N1/N5）：连 WebView2 CDP，发一条会触发压缩的长任务，在压缩窗口高频采样 DOM。
// 判据（对应 docs/smoke/2026-11-17-9af8431/REPORT.md 的 N1）：
//   压缩发生时会话区不得整块空白、输入框不得消失、压缩节点应实时出现。
//
// 用法：node scripts/dev/compact-live-probe.mjs --port 9450 --out scratch/compact-live-probe --seconds 180
// 退出码：0 = 采样窗口内未出现「composer 消失且正文为空」；1 = 复现空白；2 = 环境/交互失败。
//
// 复验前必须满足（两处都是踩过的坑，见 docs/evidence/2026-11-17/README.md 的「N1 真机复验」段）：
// 1. 数据根的 `workspace.root` 必须在**数据根之外**，否则启动弹模态「工作区设置不可用」，窗口卡在
//    初始化前（现象是「窗口在但没有 WebView2 子进程、CDP 端口不开」）；数据根还必须是干净的
//    （页面上不能已有压缩节点），否则本次采样证明不了「新发生的压缩」。
// 2. 启动 exe、跑本探针、清理进程必须在**同一条命令**内完成：命令行会话结束时子进程树会被回收。
// 参考调用（PowerShell）：
//   $psi = New-Object System.Diagnostics.ProcessStartInfo
//   $psi.FileName = '<便携包>\SHANIU.exe'; $psi.UseShellExecute = $false
//   $psi.EnvironmentVariables['SHANIU_CDP_PORT'] = '9450'
//   $psi.EnvironmentVariables['SHANIU_DATA_DIR'] = '<隔离数据根>'
//   $p = [System.Diagnostics.Process]::Start($psi); Start-Sleep -Seconds 5
//   node scripts/dev/compact-live-probe.mjs --port 9450 --out scratch/compact-live-probe --seconds 180
//   Stop-Process -Id $p.Id -Force
import { createWriteStream } from 'node:fs'
import { mkdirSync, writeFileSync } from 'node:fs'
import path from 'node:path'
import WS from 'ws'

const argv = process.argv.slice(2)
const arg = (name, fallback) => {
  const i = argv.indexOf(`--${name}`)
  return i >= 0 && i + 1 < argv.length ? argv[i + 1] : fallback
}
const port = arg('port', process.env.CDP_PORT ?? '9450')
const outDir = path.resolve(arg('out', 'scratch/compact-live-probe'))
const seconds = Number(arg('seconds', '180'))
mkdirSync(outDir, { recursive: true })

const PROMPT = '背景说明：这一段是用于触发上下文压缩的填充资料，不需要逐条回应，也不要求写入文件。\n'.repeat(95)
  + '\n任务：请用 unified_exec 执行命令 python -c "for i in range(1,2001): print(\'line\', i)"，然后把输出的总行数告诉我。'

const pages = await (await fetch(`http://127.0.0.1:${port}/json`)).json()
const page = pages.find(t => t.type === 'page')
if (page === undefined) {
  console.error(`[probe] ${port} 上没有 page 目标`)
  process.exit(2)
}
const socket = new WS(page.webSocketDebuggerUrl)
await new Promise((resolve, reject) => {
  socket.addEventListener('open', resolve, { once: true })
  socket.addEventListener('error', reject, { once: true })
})
let id = 0
const pending = new Map()
socket.addEventListener('message', event => {
  const message = JSON.parse(event.data)
  const waiter = pending.get(message.id)
  if (waiter !== undefined) { pending.delete(message.id); waiter(message) }
})
const call = (method, params = {}) => new Promise(resolve => {
  const current = ++id
  pending.set(current, resolve)
  socket.send(JSON.stringify({ id: current, method, params }))
})
const evaluate = async expression => {
  const answer = await call('Runtime.evaluate', { expression, returnByValue: true, awaitPromise: true, userGesture: true })
  if (answer.result?.exceptionDetails) throw new Error(JSON.stringify(answer.result.exceptionDetails))
  return answer.result?.result?.value
}

const snapshot = `(() => {
  const composer = document.querySelector('[contenteditable="true"]')
  const text = document.body.innerText
  const cards = document.querySelectorAll('[data-chat-call-id]').length
  return {
    composer: composer !== null,
    composerVisible: composer !== null && composer.getBoundingClientRect().height > 0,
    cards,
    bodyLen: text.length,
    hasCompaction: /上下文已压缩|已压缩上下文|已裁剪/.test(text),
    compactNode: /上下文已压缩/.test(text),
    head: text.slice(0, 160).replace(/\\n/g, ' / '),
  }
})()`

const shot = async name => {
  const answer = await call('Page.captureScreenshot', { format: 'png' })
  const data = answer.result?.data
  if (typeof data === 'string') writeFileSync(path.join(outDir, `${name}.png`), Buffer.from(data, 'base64'))
}

const samples = createWriteStream(path.join(outDir, 'samples.jsonl'), { flags: 'w' })
const record = (phase, value) => {
  const line = { t: new Date().toISOString(), phase, ...value }
  samples.write(`${JSON.stringify(line)}\n`)
  console.log(JSON.stringify(line))
}

// 1) 让输入框就绪：页面可能还在启动页/空态，轮询等待 composer；空态就先点一次「新会话」。
let ready = null
for (let i = 0; i < 80; i++) {
  ready = await evaluate(`(() => {
    const composer = document.querySelector('[contenteditable="true"]')
    if (composer === null) return { ok: false, reason: 'no-composer', text: document.body.innerText.slice(0, 300) }
    composer.focus()
    return { ok: true, tag: composer.tagName, cls: String(composer.className).slice(0, 80) }
  })()`)
  if (ready?.ok === true) break
  if (i === 2 || i === 8 || i === 16) {
    const clicked = await evaluate(`(() => {
      const button = [...document.querySelectorAll('button,[role=button]')]
        .find(element => element.textContent.trim().startsWith('新会话'))
      if (button === undefined) return false
      button.click()
      return true
    })()`)
    record('click-new-session', { clicked })
  }
  await new Promise(resolve => setTimeout(resolve, 500))
}
record('ready', ready)
if (ready?.ok !== true) {
  console.error('[probe] 输入框不可用，先确认已选中/新建会话')
  process.exit(2)
}

// 2) 真实输入前先确认页面本身没有历史压缩节点，否则本次采样证明不了「新发生的压缩」。
const initial = await evaluate(snapshot)
record('initial', initial)
if (initial?.compactNode === true) {
  console.error('[probe] 页面已有压缩节点：数据根不干净，本次不能用于 N1 判定')
  process.exit(2)
}

// 3) 真实输入：CDP insertText 让 Lexical 收到真实 input 事件，再回车发送。
await call('Input.insertText', { text: PROMPT })
record('typed', await evaluate(snapshot))
await call('Input.dispatchKeyEvent', { type: 'rawKeyDown', windowsVirtualKeyCode: 13, key: 'Enter', code: 'Enter' })
await call('Input.dispatchKeyEvent', { type: 'keyUp', windowsVirtualKeyCode: 13, key: 'Enter', code: 'Enter' })
record('sent', await evaluate(snapshot))

const deadline = Date.now() + seconds * 1000
let sawCompaction = false
let blankHits = 0
let shotIndex = 0
while (Date.now() < deadline) {
  const value = await evaluate(snapshot)
  record('sample', value)
  if (value?.compactNode === true && !sawCompaction) {
    sawCompaction = true
    await shot(`compact-window-${++shotIndex}`)
  } else if (sawCompaction && shotIndex < 5) {
    await shot(`compact-window-${++shotIndex}`)
  }
  // 「空白」判据与真机现象对齐：输入框消失 且 正文几乎为空
  if (value?.composer === false && (value?.bodyLen ?? 0) < 200) blankHits += 1
  await new Promise(resolve => setTimeout(resolve, 200))
}
samples.end()
await shot('final')
writeFileSync(path.join(outDir, 'verdict.json'), JSON.stringify({ sawCompaction, blankHits, seconds }, null, 2))
console.log(`[probe] sawCompaction=${sawCompaction} blankHits=${blankHits}`)
socket.close()
process.exit(blankHits > 0 ? 1 : sawCompaction ? 0 : 2)
