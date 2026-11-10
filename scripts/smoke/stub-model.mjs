// 冒烟用的本地 OpenAI 兼容端点：只为「发一条消息收到回复」提供确定性回答，
// 不进产物、不联网、不收发任何真实凭据。回答里带一次性 nonce，驱动脚本据此判定。
import { createServer } from 'node:http'

/**
 * @param {{ port: number, nonce: string }} opts
 * @returns {Promise<{ url: string, close: () => Promise<void> }>}
 */
export function startStub({ port, nonce }) {
  const answer = `烟测回复 ${nonce}`
  const server = createServer((req, res) => {
    const url = req.url ?? ''
    if (req.method === 'GET' && url.includes('/models')) {
      res.writeHead(200, { 'content-type': 'application/json' })
      res.end(JSON.stringify({ object: 'list', data: [{ id: 'smoke-1', object: 'model' }] }))
      return
    }
    if (req.method !== 'POST') {
      res.writeHead(405)
      res.end()
      return
    }
    let body = ''
    req.on('data', (chunk) => {
      body += chunk
    })
    req.on('end', () => {
      let stream = true
      let model = 'smoke-1'
      try {
        const parsed = JSON.parse(body)
        stream = parsed.stream !== false
        model = parsed.model ?? model
      } catch {
        // 解析不了就按默认形态回，不让 stub 自己成为红灯来源
      }
      if (!stream) {
        res.writeHead(200, { 'content-type': 'application/json' })
        res.end(
          JSON.stringify({
            id: 'chatcmpl-smoke',
            object: 'chat.completion',
            model,
            choices: [{ index: 0, message: { role: 'assistant', content: answer }, finish_reason: 'stop' }],
            usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 },
          }),
        )
        return
      }
      res.writeHead(200, {
        'content-type': 'text/event-stream',
        'cache-control': 'no-cache',
        connection: 'keep-alive',
      })
      const chunk = (delta, finish = null) =>
        `data: ${JSON.stringify({
          id: 'chatcmpl-smoke',
          object: 'chat.completion.chunk',
          model,
          choices: [{ index: 0, delta, finish_reason: finish }],
        })}\n\n`
      // 分两片发，顺带覆盖流式拼接路径
      res.write(chunk({ role: 'assistant', content: answer.slice(0, 4) }))
      res.write(chunk({ content: answer.slice(4) }))
      res.write(chunk({}, 'stop'))
      res.write(
        `data: ${JSON.stringify({
          id: 'chatcmpl-smoke',
          object: 'chat.completion.chunk',
          model,
          choices: [],
          usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 },
        })}\n\n`,
      )
      res.write('data: [DONE]\n\n')
      res.end()
    })
  })
  return new Promise((resolve, reject) => {
    server.once('error', reject)
    server.listen(port, '127.0.0.1', () => {
      resolve({
        url: `http://127.0.0.1:${port}/v1`,
        close: () => new Promise((done) => server.close(() => done())),
      })
    })
  })
}
