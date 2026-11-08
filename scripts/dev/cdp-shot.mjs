// 真机截图探针（可复用）：经 CDP 对 WebView2 页面截图，PNG 落到 scratch/ui-shots/<name>.png。
// 用途：改完前端后取真机证据（默认端口 9222，可用 CDP_PORT 覆盖）。
// 落盘位置遵循 AGENTS.md §3：测试/取证临时产物只放 %TEMP% 或 scratch/，scratch/ 卡收即清。
// 用法：node scripts/dev/cdp-shot.mjs <name>            # 全窗口
//       SHOT_CLIP=x,y,w,h node scripts/dev/cdp-shot.mjs <name>   # 局部（2x）
import { mkdirSync, writeFileSync } from 'node:fs'
import { join } from 'node:path'

const [name] = process.argv.slice(2)
if (!name) { console.error('usage: node scripts/dev/cdp-shot.mjs <name>'); process.exit(2) }
const port = process.env.CDP_PORT ?? '9222'
const list = await (await fetch(`http://127.0.0.1:${port}/json`)).json()
const page = list.find(t => t.type === 'page')
const socket = new WebSocket(page.webSocketDebuggerUrl)
await new Promise((resolve, reject) => {
  socket.addEventListener('open', resolve, { once: true })
  socket.addEventListener('error', reject, { once: true })
})
let id = 0
const pending = new Map()
socket.addEventListener('message', event => {
  const message = JSON.parse(event.data)
  const waiter = pending.get(message.id)
  if (waiter) { pending.delete(message.id); waiter(message) }
})
const call = (method, params = {}) => new Promise(resolve => {
  const current = ++id
  pending.set(current, resolve)
  socket.send(JSON.stringify({ id: current, method, params }))
})
await call('Page.bringToFront')
const clip = process.env.SHOT_CLIP
  ? (([x, y, width, height]) => ({ x, y, width, height, scale: 2 }))(process.env.SHOT_CLIP.split(',').map(Number))
  : undefined
const answer = await call('Page.captureScreenshot', { format: 'png', captureBeyondViewport: false, ...(clip ? { clip } : {}) })
const dir = join(import.meta.dirname, '..', '..', 'scratch', 'ui-shots')
mkdirSync(dir, { recursive: true })
const file = join(dir, `${name}.png`)
writeFileSync(file, Buffer.from(answer.result.data, 'base64'))
console.log(file)
socket.close()
