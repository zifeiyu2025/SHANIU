// 真机取证探针（可复用）：在 WebView2 页面里执行一段 JS 并打印返回值。
// 用途：查询 DOM/状态、验证渲染结果，取不到的值不要靠猜（默认端口 9222，可用 CDP_PORT 覆盖）。
// 用法：node scripts/dev/cdp-eval.mjs "<expression>"
const port = process.env.CDP_PORT ?? '9222'
const expression = process.argv[2]
if (!expression) {
  console.error('usage: node scripts/dev/cdp-eval.mjs "<expression>"')
  process.exit(2)
}
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
const answer = await call('Runtime.evaluate', {
  expression,
  returnByValue: true,
  awaitPromise: true,
  userGesture: true,
})
if (answer.result?.exceptionDetails) {
  console.error(JSON.stringify(answer.result.exceptionDetails, null, 1))
} else {
  console.log(JSON.stringify(answer.result?.result?.value, null, 1))
}
socket.close()