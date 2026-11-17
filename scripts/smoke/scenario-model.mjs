// 场景化本地 OpenAI 兼容端点：按剧本确定性地回文本 / tool_calls，
// 供 N2–N9 真机复验驱动使用。不进产物、不联网、不收发真实凭据。
//
// 与 stub-model.mjs 的分工：那个只回固定一句话，run-smoke.mjs 依赖它；
// 这个按 scenario.steps 逐步应答，辅助请求（会话标题、压缩摘要）不占步号。
import { createServer } from 'node:http'
import { appendFileSync } from 'node:fs'

const TITLE_MARK = 'Create a concise title for an AI coding-assistant session'
const COMPACTION_MARK = '请把上面这段对话与工具执行历史压缩成一份交接摘要'

/** 消息内容可能是字符串或多段数组 */
function textOf(content) {
  if (typeof content === 'string') return content
  if (Array.isArray(content)) {
    return content
      .map((part) => (typeof part === 'string' ? part : part?.text ?? ''))
      .join('')
  }
  return ''
}

/**
 * @param {{ port: number, scenario: { steps: unknown[], usage?: 'none'|'estimate'|'chars', delayMs?: number }, requestLog?: string }} opts
 * @returns {Promise<{ url: string, state: { mainIndex: number, overflow: boolean }, close: () => Promise<void> }>}
 */
export function startScenarioStub({ port, scenario, requestLog }) {
  const steps = [...(scenario.steps ?? [])]
  // `usage: 'none'`：模拟不回报用量的兼容端点（OpenAI 流式不传 stream_options 就是这样）。
  // 宿主于是退回字符估算判预算——这是让「只裁剪、不压缩」可复现的唯一入口。
  // `usage: 'chars'`：按请求体字符数报 prompt_tokens。中文约 1 字符 1 token，
  // `body.length / 4` 是英文口径，会把中文上下文低估 3–4 倍；组合场景要复现
  // 「观测值已达预算」这条压缩入口，必须按接近真实的分词口径报数。
  const usageMode = scenario.usage ?? 'estimate'
  const withUsage = usageMode !== 'none'
  // 应答延迟：stub 原本是零延迟，压缩窗口（摘要请求往返）在采样间隔内就结束了，
  // 看不到「窗口内界面是否空白」。给一个确定的小延迟，窗口才可观测。
  const delayMs = Math.max(0, scenario.delayMs ?? 0)
  const state = { mainIndex: 0, overflow: false, mainRequests: 0, auxRequests: 0 }

  const log = (line) => {
    if (requestLog === undefined) return
    try {
      appendFileSync(requestLog, `${JSON.stringify(line)}\n`)
    } catch {
      // 日志失败不影响应答，驱动侧另有 verdict 兜底
    }
  }

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
    req.on('end', async () => {
      if (delayMs > 0) await new Promise((resolve) => setTimeout(resolve, delayMs))
      let parsed = {}
      try {
        parsed = JSON.parse(body)
      } catch {
        // 解析不了就按空请求处理，不让 stub 自己成为红灯来源
      }
      const messages = Array.isArray(parsed.messages) ? parsed.messages : []
      const stream = parsed.stream !== false
      const model = parsed.model ?? 'smoke-1'
      const systemText = textOf(messages[0]?.content)
      const lastMessage = messages[messages.length - 1]
      const lastText = textOf(lastMessage?.content)
      const toolResultCount = messages.filter((m) => m?.role === 'tool').length

      // 辅助请求判定必须只看内容，不看顺序：标题请求和主请求可能并发到达。
      let kind = 'main'
      let payload = null
      if (systemText.includes(TITLE_MARK)) {
        kind = 'title'
        payload = { text: 'N2N9 复验', finish: 'stop' }
      } else if (lastText.includes(COMPACTION_MARK)) {
        kind = 'compaction'
        payload = { text: '（复验用压缩摘要）', finish: 'stop' }
      } else {
        payload = steps[state.mainIndex]
        if (payload === undefined) {
          state.overflow = true
          payload = { text: '（场景脚本已结束）', finish: 'stop' }
        } else {
          state.mainIndex += 1
        }
      }

      if (kind === 'main') state.mainRequests += 1
      else state.auxRequests += 1
      const promptTokens = Math.max(1, usageMode === 'chars' ? body.length : Math.ceil(body.length / 4))
      log({
        at: new Date().toISOString(),
        kind,
        mainIndex: state.mainIndex,
        messageCount: messages.length,
        toolResultCount,
        bodyChars: body.length,
        promptTokens,
        lastUserText: lastText.slice(0, 160),
        overflow: kind === 'main' && payload?.text === '（场景脚本已结束）',
      })

      const calls = Array.isArray(payload?.calls) ? payload.calls : []
      const finish = calls.length > 0 ? 'tool_calls' : (payload?.finish ?? 'stop')
      const text = typeof payload?.text === 'string' ? payload.text : ''
      // 用量必须像真的：宿主的压缩判据读端点回报的 input_tokens（SH-032），
      // 恒报 1 会让「小预算强制裁剪」永远不触发（N7 踩过）。
      const usage = {
        prompt_tokens: promptTokens,
        completion_tokens: Math.max(1, Math.ceil(text.length / 4)),
        total_tokens: 0,
      }
      usage.total_tokens = usage.prompt_tokens + usage.completion_tokens

      if (!stream) {
        res.writeHead(200, { 'content-type': 'application/json' })
        res.end(
          JSON.stringify({
            id: 'chatcmpl-scenario',
            object: 'chat.completion',
            model,
            choices: [
              {
                index: 0,
                message: {
                  role: 'assistant',
                  content: text,
                  ...(calls.length > 0
                    ? {
                        tool_calls: calls.map((call, index) => ({
                          id: call.id ?? `call-${index}`,
                          type: 'function',
                          function: { name: call.name, arguments: JSON.stringify(call.args ?? {}) },
                        })),
                      }
                    : {}),
                },
                finish_reason: finish,
              },
            ],
            ...(withUsage ? { usage } : {}),
          }),
        )
        return
      }

      res.writeHead(200, {
        'content-type': 'text/event-stream',
        'cache-control': 'no-cache',
        connection: 'keep-alive',
      })
      const chunk = (delta, reason = null) =>
        res.write(
          `data: ${JSON.stringify({
            id: 'chatcmpl-scenario',
            object: 'chat.completion.chunk',
            model,
            choices: [{ index: 0, delta, finish_reason: reason }],
          })}\n\n`,
        )

      chunk({ role: 'assistant' })
      if (calls.length > 0) {
        calls.forEach((call, index) => {
          const args = JSON.stringify(call.args ?? {})
          // 参数至少切两片：覆盖流式拼接路径，别只测「一整块到齐」。
          const cut = Math.max(1, Math.floor(args.length / 2))
          chunk({
            tool_calls: [
              {
                index,
                id: call.id ?? `call-${index}`,
                type: 'function',
                function: { name: call.name, arguments: args.slice(0, cut) },
              },
            ],
          })
          chunk({
            tool_calls: [{ index, function: { arguments: args.slice(cut) } }],
          })
        })
        chunk({}, 'tool_calls')
      } else {
        const cut = Math.max(1, Math.floor(text.length / 2))
        chunk({ content: text.slice(0, cut) })
        chunk({ content: text.slice(cut) })
        chunk({}, finish)
      }
      if (withUsage) {
        res.write(
          `data: ${JSON.stringify({
            id: 'chatcmpl-scenario',
            object: 'chat.completion.chunk',
            model,
            choices: [],
            usage,
          })}\n\n`,
        )
      }
      res.write('data: [DONE]\n\n')
      res.end()
    })
  })

  return new Promise((resolve, reject) => {
    server.once('error', reject)
    server.listen(port, '127.0.0.1', () => {
      resolve({
        url: `http://127.0.0.1:${port}/v1`,
        state,
        close: () => new Promise((done) => server.close(() => done())),
      })
    })
  })
}
