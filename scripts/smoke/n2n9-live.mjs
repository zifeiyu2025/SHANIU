// N2–N9 真机复验驱动：同一份 release exe + WebView2 CDP，按剧本发消息、断言、截图。
//
// 用法：
//   node scripts/smoke/n2n9-live.mjs --scenario n2 --exe <exe> --out <证据目录>
// 退出码：0 = 本场景全部断言通过；1 = 有断言失败；2 = 环境起不来。
//
// 前置约束（都踩过）：
// 1. 工作区根必须在数据根之外，否则启动弹「工作区设置不可用」、CDP 端口不开；
// 2. 启动 exe、跑断言、收尾要在同一条命令内（本脚本内部负责启停）；
// 3. 数据根用 SHANIU_DATA_DIR 隔离，CDP 端口用 SHANIU_CDP_PORT。
import { spawn, spawnSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import {
  appendFileSync,
  closeSync,
  copyFileSync,
  existsSync,
  mkdirSync,
  openSync,
  readFileSync,
  readdirSync,
  rmSync,
  statSync,
  writeFileSync,
} from 'node:fs'
import path from 'node:path'
import WS from 'ws'
import { startScenarioStub } from './scenario-model.mjs'

const argv = process.argv.slice(2)
const arg = (name, fallback) => {
  const i = argv.indexOf(`--${name}`)
  return i >= 0 && i + 1 < argv.length ? argv[i + 1] : fallback
}

const scenarioId = arg('scenario', 'n3')
const exe = path.resolve(arg('exe', 'dist-portable/SHANIU-Portable/SHANIU.exe'))
const outRoot = path.resolve(arg('out', 'docs/evidence/2026-11-17/n2n9-live'))
const order = ['n2', 'n3', 'n4', 'n5', 'n6', 'n7', 'n8', 'n9', 'n10']
const index = order.indexOf(scenarioId)
if (index < 0) {
  console.error(`[n2n9] 未知场景：${scenarioId}`)
  process.exit(2)
}
if (!existsSync(exe)) {
  console.error(`[n2n9] 找不到主程序：${exe}`)
  process.exit(2)
}

const scDir = path.join(outRoot, scenarioId)
const dataDir = path.join(scDir, 'data')
const workspaceDir = path.join(scDir, 'workspace')
const cdpPort = 9600 + index * 2
const stubPort = 18400 + index * 2
const nonce = `N${Date.now().toString(36).toUpperCase()}`

mkdirSync(outRoot, { recursive: true })
// 每次从干净数据根开始：残留会话会让「重开」断言指向上一轮的记录。
rmSync(scDir, { recursive: true, force: true })
mkdirSync(workspaceDir, { recursive: true })
mkdirSync(dataDir, { recursive: true })

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms))
const checks = []
const artifacts = []
const check = (name, ok, detail) => {
  checks.push({ name, ok: Boolean(ok), detail: detail ?? '' })
  console.log(`[${scenarioId}] ${ok ? 'PASS' : 'FAIL'} ${name}${detail ? ` :: ${detail}` : ''}`)
}

// ---------------------------------------------------------------------------
// 场景定义
// ---------------------------------------------------------------------------

const noteMd = '# 复验夹具\n\n这是一份用于 read_file 的说明文件。\n'
const patch = (body) => `*** Begin Patch\n${body}\n*** End Patch`

/** @type {Record<string, any>} */
const defs = {
  // N2：审批挂起时强杀，重开结局必须是 indeterminate，审批卡不复活。
  n2: {
    approval: 'on_request',
    fixtures: { 'note.md': noteMd },
    steps: [
      { calls: [{ id: `c-n2-exec-${nonce}`, name: 'unified_exec', args: { command: `echo n2-${nonce}` } }] },
      { text: '已执行。' },
    ],
    async run(d) {
      await d.sendPrompt('N2 复验：请执行一条 echo 命令，等审批时我会直接关掉程序。')
      const card = await d.waitSelector('[data-approval-key]', 45_000, '审批卡')
      const text = await d.bodyText()
      check('审批卡出现', card)
      check('显示等待审批计数', text.includes('个请求等你审批'))
      await d.shot('n2-before-kill')
      await d.stop()
      await sleep(1500)
      await d.launch()
      await d.afterLaunch()
      const opened = await d.openPriorSession('N2 复验：请执行一条 echo 命令')
      check('重开回到原会话', opened)
      await sleep(1200)
      await d.shot('n2-reopen')
      // 先把档案路径钉住：后面的强制重开可能新建空会话，不能让它抢走证据。
      const rolloutFile = d.rolloutPath()
      // 失效通知是开发者消息，落在默认折叠的过程组里；不展开就判「没留下」会误报。
      await d.expandProcessGroups()
      let body = await d.bodyText()
      const firstOpenNotice = body.includes('这条审批在程序关闭前未作答，已失效')
      if (!firstOpenNotice) {
        // 首开快照可能在宿主运行态查询就绪前落定（runtimeUnknown），按实现约定留到下一次打开。
        // 这里强制重新 follow 一次，把「下一次打开」的真实表现也拿到证据里。
        const refreshed = await d.refreshSession('N2 复验：请执行一条 echo 命令')
        check('再次打开回到原会话', refreshed)
        await sleep(1200)
        await d.shot('n2-reopen-again')
        await d.expandProcessGroups()
        body = await d.bodyText()
      }
      check('显示「本轮状态未知」', body.includes('本轮状态未知'))
      check('不出现「已停止」', !body.includes('已停止') && !body.includes('任务已停止'))
      check('审批卡不复活', !(await d.eval(`!!document.querySelector('[data-approval-key]')`)))
      check(
        '留下审批失效通知',
        body.includes('这条审批在程序关闭前未作答，已失效'),
        firstOpenNotice ? '首次打开即显示' : '首次打开未显示，重新打开后显示',
      )
      const ends = d.turnEnds(rolloutFile)
      check('rollout 结局为 indeterminate', ends.at(-1)?.reason === 'indeterminate', JSON.stringify(ends.at(-1) ?? null))
    },
  },

  // N3：截断只能有一条提示，组标题「已截断」，不得出现失败/已完成。
  n3: {
    approval: 'full_access',
    fixtures: { 'note.md': noteMd },
    steps: [{ text: '截断复验：这是一段会被上限截住的长回答，用于检查界面口径是否统一。', finish: 'length' }],
    async run(d) {
      await d.sendPrompt('N3 复验：请只回复一段话。')
      await d.waitText('截断复验：这是一段', 60_000)
      await sleep(1500)
      await d.shot('n3-live')
      await assertTruncated(d, '实时')
      await d.stop()
      await sleep(1200)
      await d.launch()
      await d.afterLaunch()
      check('重开回到原会话', await d.openPriorSession('N3 复验：请只回复一段话'))
      await sleep(1200)
      await d.shot('n3-reopen')
      await assertTruncated(d, '重开')
    },
  },

  // N4：导出带权威步数；缺权威步数时 stepsUnavailable。
  n4: {
    approval: 'full_access',
    fixtures: { 'note.md': noteMd },
    steps: [
      { calls: [{ id: `c-n4-read-${nonce}`, name: 'read_file', args: { path: 'note.md' } }] },
      { text: '第一轮完成。' },
      { calls: [{ id: `c-n4-exec-${nonce}`, name: 'unified_exec', args: { command: `echo n4-${nonce}` } }] },
      { text: '第二轮完成。' },
    ],
    async run(d) {
      await d.sendPrompt('N4 复验第一轮：读一下 note.md。')
      await d.waitText('第一轮完成。', 60_000)
      await d.sendPrompt('N4 复验第二轮：跑一条 echo。')
      await d.waitText('第二轮完成。', 60_000)
      await sleep(1000)
      await d.shot('n4-live')
      const rolloutEnds = d.turnEnds()
      const zip = path.join(outRoot, 'n4-export.zip')
      check('导出 ZIP 成功', await d.exportZip(zip))
      await d.shot('n4-export')
      const exported = d.extractSession(zip, 'n4-unzip')
      check('导出含 session.v4.jsonl', exported !== null)
      if (exported !== null) {
        writeFileSync(path.join(outRoot, 'n4-session.v4.jsonl'), exported.text)
        artifacts.push('n4-session.v4.jsonl')
        // 导出的行是 `{type, data, time, seq}`：权威步数与条目步号都在 `data` 里。
        const ends = exported.entries.filter((entry) => entry?.type === 'turn/end').map((entry) => entry.data)
        const iterations = ends.map((entry) => entry.iterations)
        check('两条 turn/end 都带数值 iterations', ends.length === 2 && iterations.every((n) => typeof n === 'number'), JSON.stringify(iterations))
        check(
          '导出 iterations 与 rollout 一致',
          ends.length === rolloutEnds.length && ends.every((entry, i) => entry.iterations === rolloutEnds[i]?.iterations),
          `export=${JSON.stringify(iterations)} rollout=${JSON.stringify(rolloutEnds.map((e) => e.iterations))}`,
        )
        const steps = exported.entries
          .map((entry) => entry?.data?.step)
          .filter((step) => typeof step === 'number')
        check('条目步号非恒 1', new Set(steps).size >= 2 && steps.includes(1), JSON.stringify(steps))
        const maxIterations = Math.max(0, ...iterations.filter((n) => typeof n === 'number'))
        check('步号不超过权威步数', steps.every((step) => step >= 0 && step < maxIterations), `steps=${JSON.stringify(steps)} max=${maxIterations}`)
        check('主导出不带 stepsUnavailable', ends.every((entry) => entry.stepsUnavailable === undefined))
      }

      // 旧档子例：把 rollout 里的权威步数抹掉，重开导出必须如实标 stepsUnavailable。
      await d.stop()
      await sleep(1200)
      const rollout = d.rolloutPath()
      check('找到 rollout 档案', rollout !== null, String(rollout))
      if (rollout !== null) {
        copyFileSync(rollout, path.join(outRoot, 'n4-rollout.jsonl'))
        artifacts.push('n4-rollout.jsonl')
        const lines = readFileSync(rollout, 'utf8').split('\n').filter(Boolean)
        const stripped = lines.map((line) => {
          try {
            const item = JSON.parse(line)
            if (item?.type === 'turn_end') delete item.iterations
            return JSON.stringify(item)
          } catch {
            return line
          }
        })
        writeFileSync(rollout, `${stripped.join('\n')}\n`)
      }
      await d.launch()
      await d.afterLaunch()
      check('旧档重开回到原会话', await d.openPriorSession('N4 复验第二轮'))
      const legacyZip = path.join(outRoot, 'n4-legacy-export.zip')
      check('旧档导出 ZIP 成功', await d.exportZip(legacyZip))
      const legacy = d.extractSession(legacyZip, 'n4-legacy-unzip')
      check('旧档导出含 session.v4.jsonl', legacy !== null)
      if (legacy !== null) {
        writeFileSync(path.join(outRoot, 'n4-legacy-session.v4.jsonl'), legacy.text)
        artifacts.push('n4-legacy-session.v4.jsonl')
        const ends = legacy.entries.filter((entry) => entry?.type === 'turn/end').map((entry) => entry.data)
        check('旧档 turn/end 标 stepsUnavailable', ends.length > 0 && ends.every((entry) => entry.stepsUnavailable === true), JSON.stringify(ends))
        check(
          '旧档条目不伪造 step',
          legacy.entries.every((entry) => entry?.data?.step === undefined),
          JSON.stringify(legacy.entries.filter((e) => e?.data?.step !== undefined).slice(0, 3)),
        )
      }
    },
  },

  // N5：挂起强杀不得把步数 +1。
  n5: {
    approval: 'on_request',
    fixtures: { 'note.md': noteMd },
    steps: [
      { text: '第一轮完成。' },
      { calls: [{ id: `c-n5-read-${nonce}`, name: 'read_file', args: { path: 'note.md' } }] },
      { text: '第二轮完成。' },
      { calls: [{ id: `c-n5-exec-${nonce}`, name: 'unified_exec', args: { command: `echo n5-${nonce}` } }] },
      { text: '不会执行。' },
    ],
    async run(d) {
      await d.sendPrompt('N5 复验第一轮：只回一句话。')
      await d.waitText('第一轮完成。', 60_000)
      await d.sendPrompt('N5 复验第二轮：读一下 note.md。')
      await d.waitText('第二轮完成。', 60_000)
      await sleep(800)
      const before = await d.footer()
      // 第 1 轮 1 步、第 2 轮 read+收尾 2 步：前置就是 2 轮 3 步。
      check('前置为 2 轮 3 步', before !== null && before[0] === 2 && before[1] === 3, JSON.stringify(before))
      await d.shot('n5-before-kill')
      const rolloutBefore = d.rolloutPath()
      if (rolloutBefore !== null) {
        copyFileSync(rolloutBefore, path.join(outRoot, 'n5-rollout-before.jsonl'))
        artifacts.push('n5-rollout-before.jsonl')
      }
      await d.sendPrompt('N5 复验第三轮：再跑一条 echo。')
      const card = await d.waitSelector('[data-approval-key]', 45_000, '审批卡')
      check('第三轮审批卡出现', card)
      await d.stop()
      await sleep(1500)
      await d.launch()
      await d.afterLaunch()
      check('重开回到原会话', await d.openPriorSession('N5 复验第三轮：再跑一条 echo。'))
      await sleep(1200)
      await d.shot('n5-reopen')
      const after = await d.footer()
      // 判据是「挂起回合不得 +1」：与强杀前的页脚逐项相等，而不是钉死一个数字。
      check(
        '重开后步数未 +1',
        after !== null && before !== null && after[0] === before[0] && after[1] === before[1],
        `before=${JSON.stringify(before)} after=${JSON.stringify(after)}`,
      )
      const ends = d.turnEnds()
      // 挂起的第 3 轮要如实记一条 indeterminate（N2 口径），但不能带 iterations——没采样过就没有步数。
      check(
        '挂起回合落 indeterminate 且不伪造步数',
        ends.length === 3 && ends[2]?.reason === 'indeterminate' && ends[2]?.iterations === undefined,
        JSON.stringify(ends.map((e) => ({ turn: e.turn, reason: e.reason, iterations: e.iterations }))),
      )
      await d.expandProcessGroups()
      const body = await d.bodyText()
      check('显示「本轮状态未知」', body.includes('本轮状态未知'))
      check('不出现「已停止」', !body.includes('已停止') && !body.includes('任务已停止'))
      const rolloutAfter = d.rolloutPath()
      if (rolloutAfter !== null) {
        copyFileSync(rolloutAfter, path.join(outRoot, 'n5-rollout-after.jsonl'))
        artifacts.push('n5-rollout-after.jsonl')
      }
    },
  },

  // N6：越界/用户拒绝的卡折叠态不得是 ok；正文里含同样文字的负例不得误判。
  n6: {
    approval: 'on_request',
    fixtures: {
      'marker.txt': '说明：路径越界（工作区之外）: 这是文档示例，不是拒绝\n备注：拒绝说明：type=user_denied；这是示例\n',
    },
    steps: [
      { calls: [{ id: 'c-n6-out', name: 'read_file', args: { path: '../outside.txt' } }] },
      { calls: [{ id: 'c-n6-marker', name: 'read_file', args: { path: 'marker.txt' } }] },
      { calls: [{ id: 'c-n6-deny', name: 'unified_exec', args: { command: `echo n6-${nonce}` } }] },
      { text: '已处理。' },
    ],
    async run(d) {
      writeFileSync(path.join(scDir, 'outside.txt'), '工作区之外的夹具文件。\n')
      await d.sendPrompt('N6 复验：先读工作区外的文件，再读 marker，最后跑一条需要审批的命令。')
      check('越界 read 卡出现', await d.waitSelector('[data-chat-call-id="c-n6-out"]', 45_000, 'c-n6-out'))
      check('负例 read 卡出现', await d.waitSelector('[data-chat-call-id="c-n6-marker"]', 45_000, 'c-n6-marker'))
      const card = await d.waitSelector('[data-approval-key]', 45_000, '审批卡')
      check('审批卡出现', card)
      const rejected = await d.eval(`(() => {
        const root = document.querySelector('[data-approval-key]')
        if (!root) return false
        const button = [...root.querySelectorAll('button')].find((node) => /拒绝|Reject/i.test(node.innerText || ''))
        if (!button) return false
        button.click()
        return true
      })()`)
      check('点击「拒绝」', rejected)
      await d.waitText('已处理。', 60_000)
      await sleep(1000)
      await d.shot('n6-live')
      // 三张卡都在回合过程组里，组默认折叠时卡片根本不在 innerText 中。
      await d.expandProcessGroups()
      const live = await d.collectCards(['c-n6-out', 'c-n6-marker', 'c-n6-deny'])
      writeFileSync(path.join(outRoot, 'n6-cards-live.json'), JSON.stringify(live, null, 2))
      artifacts.push('n6-cards-live.json')
      assertCards(live, '实时')
      await d.stop()
      await sleep(1200)
      await d.launch()
      await d.afterLaunch()
      check('重开回到原会话', await d.openPriorSession('N6 复验：先读工作区外的文件'))
      await sleep(1200)
      await d.shot('n6-reopen')
      await d.expandProcessGroups()
      const reopen = await d.collectCards(['c-n6-out', 'c-n6-marker', 'c-n6-deny'])
      writeFileSync(path.join(outRoot, 'n6-cards-reopen.json'), JSON.stringify(reopen, null, 2))
      artifacts.push('n6-cards-reopen.json')
      assertCards(reopen, '重开')
    },
  },

  // N7：D6 裁剪要在实时界面、重开卡片、导出三处都留痕，档案保留裁剪前原文。
  n7: {
    approval: 'full_access',
    tokenBudget: 5000,
    // 不回报用量：宿主退回字符估算，裁剪后即回预算内 → 只裁剪不压缩，回合才留在视图里。
    usage: 'none',
    // 命令走 cmd /C，内联 -c 的引号会被多一层解释打掉，改成跑工作区里的脚本。
    fixtures: { 'note.md': noteMd, 'big.py': "for i in range(1, 4001):\n    print('line', i)\n" },
    steps: [
      {
        calls: [
          { id: 'c-n7-big', name: 'unified_exec', args: { command: 'python big.py' } },
        ],
      },
      { text: '命令完成。' },
    ],
    async run(d) {
      await d.sendPrompt('N7 复验：请执行一段会输出很多行的命令。')
      await d.waitText('命令完成。', 120_000)
      await d.shot('n7-live')
      await d.expandProcessGroups()
      let notice = false
      for (let i = 0; i < 40 && !notice; i += 1) {
        const body = await d.bodyText()
        notice = body.includes('的输出在发给模型前被确定性裁剪') && body.includes('卡片里显示的仍是裁剪前原文')
        if (!notice) await sleep(500)
      }
      check('实时界面有裁剪通知', notice)
      await d.shot('n7-live-expanded')
      const rollout = d.rolloutPath()
      check('找到 rollout 档案', rollout !== null)
      if (rollout !== null) {
        copyFileSync(rollout, path.join(outRoot, 'n7-rollout.jsonl'))
        artifacts.push('n7-rollout.jsonl')
        const raw = readFileSync(rollout, 'utf8')
        const big = raw.split('\n').find((line) => line.includes('line 4000')) ?? ''
        check('档案保留裁剪前原文', big.length > 8192 && big.includes('line 4000'), `len=${big.length}`)
      }
      await d.stop()
      await sleep(1200)
      await d.launch()
      await d.afterLaunch()
      check('重开回到原会话', await d.openPriorSession('N7 复验：请执行一段会输出很多行的命令'))
      await sleep(1200)
      await d.expandProcessGroups()
      const cardText = await d.expandCard('c-n7-big')
      await d.shot('n7-reopen')
      check('重开卡片有判读行', cardText?.includes('[已裁剪]') === true && cardText.includes('裁剪前原文（完整）'), String(cardText ?? '').slice(0, 120))
      check('重开卡片仍保留原文尾部', cardText?.includes('line 4000') === true)
      const zip = path.join(outRoot, 'n7-export.zip')
      check('导出 ZIP 成功', await d.exportZip(zip))
      await d.shot('n7-export')
      const exported = d.extractSession(zip, 'n7-unzip')
      check('导出含 session.v4.jsonl', exported !== null)
      if (exported !== null) {
        writeFileSync(path.join(outRoot, 'n7-session.v4.jsonl'), exported.text)
        artifacts.push('n7-session.v4.jsonl')
        const count = (exported.text.match(/已裁剪/g) ?? []).length
        check('导出含「已裁剪」留痕', count > 0, `count=${count}`)
        check('导出保留裁剪前原文', exported.text.includes('line 4000'))
      }
    },
  },

  // N8：多块补丁第 2 块失败时，回执里的「实际读到」必须是磁盘原文。
  n8: {
    approval: 'full_access',
    fixtures: { 'data.txt': 'a1\na2\na3\n' },
    steps: [
      // apply_patch 要求本回合先读过目标文件；没读会先撞 write_file_not_read，验不到正文回执。
      { calls: [{ id: 'c-n8-read', name: 'read_file', args: { path: 'data.txt' } }] },
      {
        calls: [
          {
            id: 'c-n8-patch',
            name: 'apply_patch',
            args: {
              patch: patch(
                [
                  '*** Update File: data.txt',
                  '@@',
                  '-a1',
                  '+a1-first',
                  '@@',
                  ' a1-first',
                  '-nope',
                  '+nope2',
                ].join('\n'),
              ),
            },
          },
        ],
      },
      { text: '补丁已处理。' },
    ],
    async run(d) {
      await d.sendPrompt('N8 复验：请用一次 apply_patch 改两处。')
      await d.waitText('补丁已处理。', 60_000)
      await sleep(800)
      await d.expandProcessGroups()
      const text = await d.expandCard('c-n8-patch')
      await d.shot('n8-live')
      const state = await d.cardState('c-n8-patch')
      writeFileSync(
        path.join(outRoot, 'n8-card.json'),
        JSON.stringify({ state, text }, null, 2),
      )
      artifacts.push('n8-card.json')
      check('补丁卡为错误态', state !== null && state !== 'ok', String(state))
      // 回执正文随事件到达实时卡（此前只有重开读档案才看得到）。
      check('实时回执「实际读到」为磁盘原文', text?.includes('实际读到 `a1`') === true, String(text ?? '').slice(0, 300))
      check('实时回执不含预演后的 a1-first', text?.includes('实际读到 `a1-first`') !== true)
      await d.stop()
      await sleep(1200)
      await d.launch()
      await d.afterLaunch()
      check('重开回到原会话', await d.openPriorSession('N8 复验：请用一次 apply_patch 改两处。'))
      await sleep(1200)
      await d.expandProcessGroups()
      const reopenText = await d.expandCard('c-n8-patch')
      await d.shot('n8-reopen')
      // 补丁卡的正文不挂在 `[data-chat-call-id]` 内部（该节点只有标题行），按整页可见文本判。
      const reopenBody = await d.fullText()
      writeFileSync(path.join(outRoot, 'n8-reopen-text.txt'), reopenBody)
      artifacts.push('n8-reopen-text.txt')
      writeFileSync(
        path.join(outRoot, 'n8-card-reopen.json'),
        JSON.stringify({ state: await d.cardState('c-n8-patch'), text: reopenText }, null, 2),
      )
      artifacts.push('n8-card-reopen.json')
      check('回执「实际读到」为磁盘原文', reopenBody.includes('实际读到 `a1`'), reopenBody.slice(-400))
      check('回执不含预演后的 a1-first', !reopenBody.includes('实际读到 `a1-first`'))
      const after = readFileSync(path.join(workspaceDir, 'data.txt'), 'utf8')
      writeFileSync(path.join(outRoot, 'n8-data-after.txt'), after)
      artifacts.push('n8-data-after.txt')
      check('失败后整单回滚', after === 'a1\na2\na3\n', JSON.stringify(after))
    },
  },

  // N9：第 1 回合 read+patch，第 2 回合直接 patch 仍拒绝，但文案不得说「未读取过」。
  n9: {
    approval: 'full_access',
    fixtures: { 'note.txt': 'alpha\nbeta\n' },
    steps: [
      { calls: [{ id: 'c-n9-read', name: 'read_file', args: { path: 'note.txt' } }] },
      {
        calls: [
          {
            id: 'c-n9-patch-1',
            name: 'apply_patch',
            args: { patch: patch(['*** Update File: note.txt', '@@', '-alpha', '+alpha-1'].join('\n')) },
          },
        ],
      },
      { text: '已修改。' },
      {
        calls: [
          {
            id: 'c-n9-patch-2',
            name: 'apply_patch',
            args: { patch: patch(['*** Update File: note.txt', '@@', '-alpha-1', '+alpha-2'].join('\n')) },
          },
        ],
      },
      { text: '第二次未写。' },
    ],
    async run(d) {
      await d.sendPrompt('N9 复验第一轮：读 note.txt 再改一处。')
      await d.waitText('已修改。', 60_000)
      await d.sendPrompt('N9 复验第二轮：直接再改一处，不用重新读。')
      await d.waitText('第二次未写。', 60_000)
      await sleep(800)
      await d.expandProcessGroups()
      const text = await d.expandCard('c-n9-patch-2')
      await d.shot('n9-live')
      const state = await d.cardState('c-n9-patch-2')
      writeFileSync(path.join(outRoot, 'n9-card.json'), JSON.stringify({ state, text }, null, 2))
      artifacts.push('n9-card.json')
      check('第二次补丁卡为错误态', state !== null && state !== 'ok', String(state))
      // 实时卡也拿得到失败文案（此前只报错误码），与重开口径一致。
      check(
        '实时文案指向「读取记录过期」',
        text?.includes('没有可用的读取记录') === true,
        String(text ?? '').slice(0, 300),
      )
      check('实时不出现「未读取过」', text?.includes('未读取过') !== true)
      await d.stop()
      await sleep(1200)
      await d.launch()
      await d.afterLaunch()
      check('重开回到原会话', await d.openPriorSession('N9 复验第二轮：直接再改一处，不用重新读。'))
      await sleep(1200)
      await d.expandProcessGroups()
      const reopenText = await d.expandCard('c-n9-patch-2')
      await d.shot('n9-reopen')
      writeFileSync(
        path.join(outRoot, 'n9-card-reopen.json'),
        JSON.stringify({ state: await d.cardState('c-n9-patch-2'), text: reopenText }, null, 2),
      )
      artifacts.push('n9-card-reopen.json')
      check(
        '文案指向「读取记录过期」',
        reopenText?.includes(
          'note.txt 在本任务内没有可用的读取记录（可能尚未读取，或上次读取发生在本次修改之前、内容可能已过期）',
        ) === true,
        String(reopenText ?? '').slice(0, 300),
      )
      check('不出现「未读取过」', reopenText?.includes('未读取过') !== true)
      const after = readFileSync(path.join(workspaceDir, 'note.txt'), 'utf8')
      writeFileSync(path.join(outRoot, 'n9-note-after.txt'), after)
      artifacts.push('n9-note-after.txt')
      check('文件保持第 1 回合结果', after === 'alpha-1\nbeta\n', JSON.stringify(after))
    },
  },

  // N10：组合链路——小预算 → 大块工具输出 → 裁剪 + 压缩 → 追问 → 重开 → 导出。
  // N2–N9 是把每个修复拆开验的，这里只验它们叠在一起时是否互相破坏：
  // 压缩窗口不空白、压缩节点实时出现、轮次步数在界面/档案/导出三处一致、导出带裁剪留痕。
  // 用量按字符数回报（中文口径）：观测值才会真的越预算，走到「裁剪 + 摘要」组合路径。
  // delayMs 让摘要往返可观测，否则零延迟 stub 会在一个采样间隔内跑完整条链路。
  n10: {
    approval: 'full_access',
    tokenBudget: 5000,
    usage: 'chars',
    delayMs: 300,
    fixtures: { 'note.md': noteMd, 'big.py': "for i in range(1, 4001):\n    print('line', i)\n" },
    steps: [
      { calls: [{ id: 'c-n10-big', name: 'unified_exec', args: { command: 'python big.py' } }] },
      { text: '第一轮完成。' },
      { text: '追问完成。' },
    ],
    async run(d) {
      const samples = []
      let sawCompaction = false
      let blankHits = 0
      let compactBlank = 0
      await d.sendPrompt('N10 组合复验第一轮：请执行一段会输出很多行的命令，我会在压缩窗口里观察界面。')
      const deadline = Date.now() + 180_000
      while (Date.now() < deadline) {
        const snap = await d.snapshot('第一轮完成。')
        samples.push({ at: new Date().toISOString(), ...snap })
        if (snap.compactNode && !sawCompaction) {
          sawCompaction = true
          await d.shot('n10-compaction-live')
        }
        if (!snap.composer && snap.bodyLen < 200) blankHits += 1
        if (snap.compactNode && (!snap.composer || !snap.composerVisible)) compactBlank += 1
        if (snap.marker) break
        await sleep(200)
      }
      writeFileSync(
        path.join(outRoot, 'n10-samples.jsonl'),
        `${samples.map((item) => JSON.stringify(item)).join('\n')}\n`,
      )
      artifacts.push('n10-samples.jsonl')
      check('第一轮在窗口内完成', samples.at(-1)?.marker === true, `samples=${samples.length}`)
      check('采样窗口内确实发生压缩', sawCompaction, `compactSamples=${samples.filter((item) => item.compactNode).length}`)
      check('压缩窗口输入框未消失', compactBlank === 0, `hits=${compactBlank}`)
      check('压缩窗口未整块空白', blankHits === 0, `hits=${blankHits}`)

      await d.sendPrompt('N10 组合复验第二轮：用一句话说明当前进度。')
      await d.waitText('追问完成。', 120_000)
      await sleep(1200)
      await d.shot('n10-live-final')
      // 裁剪通知与压缩节点都在过程组里：先展开，再按可见文本判「实时就有」。
      await d.expandProcessGroups()
      const liveBody = await d.bodyText()
      writeFileSync(path.join(outRoot, 'n10-live-text.txt'), liveBody)
      artifacts.push('n10-live-text.txt')
      check(
        '实时界面有裁剪通知',
        liveBody.includes('的输出在发给模型前被确定性裁剪') && liveBody.includes('卡片里显示的仍是裁剪前原文'),
      )
      check('实时界面出现压缩节点', liveBody.includes('上下文已压缩'))
      const liveFooter = await d.footer()
      check('实时页脚为 2 轮', liveFooter !== null && liveFooter[0] === 2, JSON.stringify(liveFooter))

      const zip = path.join(outRoot, 'n10-export.zip')
      check('导出 ZIP 成功', await d.exportZip(zip))
      await d.shot('n10-export')
      const exported = d.extractSession(zip, 'n10-unzip')
      check('导出含 session.v4.jsonl', exported !== null)
      if (exported !== null) {
        writeFileSync(path.join(outRoot, 'n10-session.v4.jsonl'), exported.text)
        artifacts.push('n10-session.v4.jsonl')
        const ends = exported.entries.filter((entry) => entry?.type === 'turn/end').map((entry) => entry.data)
        const iterations = ends.map((entry) => entry.iterations)
        const turnStarts = exported.entries.filter((entry) => entry?.type === 'turn/start').length
        const steps = exported.entries.map((entry) => entry?.data?.step).filter((step) => typeof step === 'number')
        const prunedHits = (exported.text.match(/已裁剪/g) ?? []).length
        const compactionEvents = exported.entries.filter((entry) => String(entry?.type ?? '').startsWith('compaction/')).length
        const sumIterations = iterations.filter((n) => typeof n === 'number').reduce((total, n) => total + n, 0)
        const maxIterations = Math.max(0, ...iterations.filter((n) => typeof n === 'number'))
        check('导出含「已裁剪」留痕', prunedHits > 0, `count=${prunedHits}`)
        check('导出含压缩事件', compactionEvents > 0, `count=${compactionEvents}`)
        check(
          '导出回合数与界面一致',
          liveFooter !== null && turnStarts === liveFooter[0] && ends.length === liveFooter[0],
          `starts=${turnStarts} ends=${ends.length} ui=${JSON.stringify(liveFooter)}`,
        )
        check('导出 turn/end 都带数值 iterations', ends.length > 0 && iterations.every((n) => typeof n === 'number'), JSON.stringify(iterations))
        check('导出步数合计与界面一致', liveFooter !== null && sumIterations === liveFooter[1], `export=${sumIterations} ui=${JSON.stringify(liveFooter)}`)
        check(
          '导出条目步号非恒 1 且不越界',
          new Set(steps).size >= 2 && steps.includes(1) && steps.every((step) => step >= 0 && step < maxIterations),
          JSON.stringify(steps),
        )
        check('主导出不带 stepsUnavailable', ends.every((entry) => entry.stepsUnavailable === undefined))
      }

      await d.stop()
      await sleep(1500)
      await d.launch()
      await d.afterLaunch()
      // 第一轮的提问会被压缩摘要吞掉，重开后不再可见；用第二轮提问判「回到了原会话」。
      check('重开回到原会话', await d.openPriorSession('N10 组合复验第二轮'))
      await sleep(1500)
      await d.expandProcessGroups()
      const reopenBody = await d.bodyText()
      writeFileSync(path.join(outRoot, 'n10-reopen-text.txt'), reopenBody)
      artifacts.push('n10-reopen-text.txt')
      await d.shot('n10-reopen')
      check('重开后压缩节点在', reopenBody.includes('上下文已压缩'))
      const reopenFooter = await d.footer()
      check(
        '重开轮次步数与实时一致',
        reopenFooter !== null && liveFooter !== null && reopenFooter[0] === liveFooter[0] && reopenFooter[1] === liveFooter[1],
        `live=${JSON.stringify(liveFooter)} reopen=${JSON.stringify(reopenFooter)}`,
      )
      const ends = d.turnEnds()
      const sumIterations = ends.reduce((total, entry) => total + (typeof entry.iterations === 'number' ? entry.iterations : 0), 0)
      check(
        '档案轮次与界面一致',
        reopenFooter !== null && ends.length === reopenFooter[0],
        JSON.stringify(ends.map((entry) => ({ turn: entry.turn, reason: entry.reason, iterations: entry.iterations }))),
      )
      check('档案步数合计与界面一致', reopenFooter !== null && sumIterations === reopenFooter[1], `sum=${sumIterations} ui=${JSON.stringify(reopenFooter)}`)
    },
  },
}

function assertTruncated(d, phase) {
  return d.bodyText().then((body) => {
    const hits = (body.match(/被截断/g) ?? []).length
    check(`${phase}：截断提示恰好一条`, hits === 1, `count=${hits}`)
    check(`${phase}：组标题为「已截断」`, body.includes('已截断'))
    check(`${phase}：无「本轮运行失败」`, !body.includes('本轮运行失败'))
    check(`${phase}：无「已完成」`, !body.includes('已完成'))
  })
}

function assertCards(cards, phase) {
  const out = cards['c-n6-out']
  const marker = cards['c-n6-marker']
  const deny = cards['c-n6-deny']
  check(`${phase}：越界卡不是 ok`, typeof out?.state === 'string' && out.state !== 'ok', JSON.stringify(out))
  check(`${phase}：拒绝卡不是 ok`, typeof deny?.state === 'string' && deny.state !== 'ok', JSON.stringify(deny))
  check(`${phase}：负例卡仍为 ok`, marker?.state === 'ok', JSON.stringify(marker))
  // 完整拒绝说明两视图都必须有：实时事件带 error 正文，重开由档案 output 回放。
  check(`${phase}：越界卡保留拒绝说明`, out?.text?.includes('路径越界') === true, String(out?.text ?? '').slice(0, 120))
  check(`${phase}：拒绝卡保留拒绝说明`, deny?.text?.includes('拒绝说明：type=') === true, String(deny?.text ?? '').slice(0, 120))
  if (phase !== '重开') {
    check(`${phase}：越界卡显式标失败`, out?.text?.includes('失败') === true, JSON.stringify(out?.text?.slice(0, 80)))
    check(`${phase}：拒绝卡显式标失败`, deny?.text?.includes('失败') === true, JSON.stringify(deny?.text?.slice(0, 80)))
  }
}

const def = defs[scenarioId]
if (def === undefined) {
  console.error(`[n2n9] 场景未实现：${scenarioId}`)
  process.exit(2)
}

// 夹具：工作区内的写进 workspace，工作区外的一律写 scDir。
for (const [name, content] of Object.entries(def.fixtures ?? {})) {
  writeFileSync(path.join(workspaceDir, name), content)
}

const approval = def.approval ?? 'full_access'
const sandbox = 'workspace_write'
// D6 确定性裁剪只在上下文超出预算时发生：N7 必须用小预算，否则 2 万字符也照发。
const tokenBudget = def.tokenBudget ?? 500000
writeFileSync(
  path.join(dataDir, 'config.toml'),
  [
    'active_provider = "smoke"',
    '',
    '[workspace]',
    `root = "${workspaceDir.replaceAll('\\', '/')}"`,
    '',
    '[approval]',
    `mode = "${approval}"`,
    '',
    '[sandbox]',
    `mode = "${sandbox}"`,
    '',
    '[agent]',
    `token_budget = ${tokenBudget}`,
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

const requestLog = path.join(outRoot, `${scenarioId}-requests.jsonl`)
if (existsSync(requestLog)) rmSync(requestLog)

const stub = await startScenarioStub({
  port: stubPort,
  scenario: { steps: def.steps, usage: def.usage, delayMs: def.delayMs },
  requestLog,
})

// ---------------------------------------------------------------------------
// CDP 驱动
// ---------------------------------------------------------------------------

// 原生保存对话框的自动化（Windows Common Item Dialog：顶层 `#32770`，名字输入框是最宽的
// 子 `Edit`，确认按钮是文本含「保存」的子 `Button`）。不依赖前台焦点。
const SAVE_DIALOG_SCRIPT = String.raw`
param([string]$Dest, [int]$WaitSeconds = 25)
Write-Output 'start'
Add-Type -AssemblyName System.Windows.Forms
Add-Type @"
using System;
using System.Runtime.InteropServices;
using System.Text;
public class W {
  public delegate bool EnumWindowsProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc cb, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumChildWindows(IntPtr p, EnumWindowsProc cb, IntPtr l);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern int GetClassName(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern int GetWindowText(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern IntPtr SendMessage(IntPtr h, uint m, IntPtr w, string l);
  [DllImport("user32.dll")] public static extern IntPtr SendMessage(IntPtr h, uint m, IntPtr w, IntPtr l);
  public struct RECT { public int Left, Top, Right, Bottom; }
}
"@
Write-Output 'compiled'
$script:target = [IntPtr]::Zero
$script:others = @()
$deadline = (Get-Date).AddSeconds($WaitSeconds)
while ((Get-Date) -lt $deadline -and $script:target -eq [IntPtr]::Zero) {
  $script:others = @()
  [W]::EnumWindows({ param($h, $l)
    if (-not [W]::IsWindowVisible($h)) { return $true }
    $cls = New-Object System.Text.StringBuilder 256
    [void][W]::GetClassName($h, $cls, 256)
    if ($cls.ToString() -eq '#32770') {
      $title = New-Object System.Text.StringBuilder 512
      [void][W]::GetWindowText($h, $title, 512)
      $t = $title.ToString()
      $script:others += ('#32770|' + $t)
      if ($t -match '保存|Save|另存') { $script:target = $h; return $false }
    }
    return $true
  }, [IntPtr]::Zero) | Out-Null
  if ($script:target -eq [IntPtr]::Zero) { Start-Sleep -Milliseconds 400 }
}
Write-Output ('candidates=' + ($script:others -join ' ; '))
if ($script:target -eq [IntPtr]::Zero) { Write-Output 'no-dialog'; exit 3 }
Write-Output ('dialog=' + $script:target)
$script:bestEdit = [IntPtr]::Zero
$script:bestWidth = 0
$script:button = [IntPtr]::Zero
[W]::EnumChildWindows($script:target, { param($h, $l)
  $cls = New-Object System.Text.StringBuilder 256
  [void][W]::GetClassName($h, $cls, 256)
  $c = $cls.ToString()
  if ($c -eq 'Edit') {
    $r = New-Object W+RECT
    [void][W]::GetWindowRect($h, [ref]$r)
    $w = $r.Right - $r.Left
    if ($w -gt $script:bestWidth) { $script:bestWidth = $w; $script:bestEdit = $h }
  }
  if ($c -eq 'Button') {
    $t = New-Object System.Text.StringBuilder 256
    [void][W]::GetWindowText($h, $t, 256)
    if ($t.ToString() -match '保存|Save') { $script:button = $h }
  }
  return $true
}, [IntPtr]::Zero) | Out-Null
Write-Output ('edit=' + $script:bestEdit + ' width=' + $script:bestWidth + ' button=' + $script:button)
if ($script:bestEdit -ne [IntPtr]::Zero) {
  [void][W]::SendMessage($script:bestEdit, 0x000C, [IntPtr]::Zero, $Dest)
  Start-Sleep -Milliseconds 400
}
if ($script:button -ne [IntPtr]::Zero) {
  [void][W]::SendMessage($script:button, 0x00F5, [IntPtr]::Zero, [IntPtr]::Zero)
  Write-Output 'clicked'
  exit 0
}
[void][W]::SetForegroundWindow($script:target)
Start-Sleep -Milliseconds 500
$fg = [W]::GetForegroundWindow()
Write-Output ('fg=' + $fg)
if ($fg -ne $script:target) { Write-Output 'fg-mismatch'; exit 4 }
[System.Windows.Forms.SendKeys]::SendWait('^a')
[System.Windows.Forms.SendKeys]::SendWait($Dest)
Start-Sleep -Milliseconds 400
[System.Windows.Forms.SendKeys]::SendWait('{ENTER}')
Write-Output 'typed'
exit 0
`

class Driver {
  constructor() {
    this.child = null
    this.sock = null
    this.nextId = 0
    this.pending = new Map()
    this.childExit = null
  }

  async launch() {
    this.childExit = null
    this.child = spawn(exe, [], {
      env: {
        ...process.env,
        SHANIU_CDP_PORT: String(cdpPort),
        SHANIU_DATA_DIR: dataDir,
      },
      stdio: ['ignore', 'pipe', 'pipe'],
      windowsHide: true,
    })
    for (const stream of [this.child.stdout, this.child.stderr]) {
      stream?.on('data', (chunk) => appendFileSync(path.join(outRoot, `${scenarioId}-app.txt`), chunk))
    }
    this.child.on('exit', (code, signal) => {
      this.childExit = { code, signal }
    })
    this.child.on('error', (error) => {
      this.childExit = { error: String(error) }
    })

    const page = await this.waitForTarget(90_000)
    this.sock = new WS(page.webSocketDebuggerUrl, { perMessageDeflate: false })
    await new Promise((resolve, reject) => {
      this.sock.once('open', resolve)
      this.sock.once('error', reject)
    })
    this.sock.on('message', (raw) => {
      const message = JSON.parse(raw.toString())
      const waiter = this.pending.get(message.id)
      if (waiter !== undefined) {
        this.pending.delete(message.id)
        waiter(message)
      }
    })
    await this.send('Page.enable')
    await this.send('Runtime.enable')
  }

  async waitForTarget(timeoutMs) {
    const deadline = Date.now() + timeoutMs
    let last = null
    while (Date.now() < deadline) {
      if (this.childExit !== null) {
        throw new Error(`主程序已退出（${JSON.stringify(this.childExit)}），CDP 目标不会出现`)
      }
      try {
        const response = await fetch(`http://127.0.0.1:${cdpPort}/json/list`, {
          signal: AbortSignal.timeout(2000),
        })
        last = await response.json()
        const page = last.find((target) => target.type === 'page' && typeof target.webSocketDebuggerUrl === 'string')
        if (page !== undefined) return page
      } catch (error) {
        last = String(error)
      }
      await sleep(500)
    }
    throw new Error(`CDP 目标未出现：${JSON.stringify(last)}`)
  }

  send(method, params = {}) {
    return new Promise((resolve, reject) => {
      const id = ++this.nextId
      this.pending.set(id, (message) =>
        message.error ? reject(new Error(`${method}: ${JSON.stringify(message.error)}`)) : resolve(message.result),
      )
      this.sock.send(JSON.stringify({ id, method, params }))
    })
  }

  async eval(expression) {
    const result = await this.send('Runtime.evaluate', { expression, returnByValue: true, awaitPromise: true })
    if (result.exceptionDetails) throw new Error(`页面脚本异常：${result.exceptionDetails.text}`)
    return result.result?.value
  }

  async waitFor(expression, timeoutMs, label) {
    const deadline = Date.now() + timeoutMs
    while (Date.now() < deadline) {
      const value = await this.eval(expression).catch(() => null)
      if (value) return value
      await sleep(400)
    }
    throw new Error(`等待超时 ${timeoutMs}ms：${label}`)
  }

  async waitSelector(selector, timeoutMs, label) {
    return (await this.waitFor(`!!document.querySelector(${JSON.stringify(selector)})`, timeoutMs, label).catch(() => false)) !== false
  }

  async waitText(text, timeoutMs = 60_000) {
    return this.waitFor(
      `(document.body.innerText || '').includes(${JSON.stringify(text)})`,
      timeoutMs,
      `正文含「${text}」`,
    )
  }

  bodyText() {
    return this.eval(`document.body.innerText || ''`)
  }

  // 压缩窗口的窗口期只有一次摘要往返，200ms 采样才能看清「界面是否整块空白」。
  // 判据取自 N1 探针：composer 不在 且 正文几乎为空 = 空白；压缩节点按可见文本判。
  snapshot(marker = '') {
    return this.eval(`(() => {
      const composer = document.querySelector('[contenteditable="true"]')
      const text = document.body.innerText || ''
      return {
        composer: composer !== null,
        composerVisible: composer !== null && composer.getBoundingClientRect().height > 0,
        bodyLen: text.length,
        compactNode: /上下文已压缩/.test(text),
        pruneNotice: text.includes('的输出在发给模型前被确定性裁剪'),
        marker: ${marker === '' ? 'true' : `text.includes(${JSON.stringify(marker)})`},
        head: text.slice(0, 120).replace(/\\n/g, ' / '),
      }
    })()`)
  }

  // 工具卡正文用 content-visibility 做了屏外跳过，`innerText` 会漏掉；判文案用 `textContent`
  // 并限定在聊天流内，避免把引导脚本的文本也算进来。
  fullText() {
    return this.eval(`(() => { const el = document.querySelector('[data-chat-flow]'); return (el || document.body).textContent || '' })()`)
  }

  async shot(tag) {
    try {
      const result = await this.send('Page.captureScreenshot', { format: 'png' })
      writeFileSync(path.join(outRoot, `${tag}.png`), Buffer.from(result.data, 'base64'))
      artifacts.push(`${tag}.png`)
    } catch {
      // 截图失败不影响判定
    }
  }

  async storeKey() {
    return this.eval(
      `(async () => { try { await window.__TAURI_INTERNALS__.invoke('store_api_key', { provider: 'SMOKE_KEY', key: 'smoke-key' }); return true } catch (error) { return String(error) } })()`,
    )
  }

  async afterLaunch() {
    const stored = await this.storeKey()
    if (stored !== true) throw new Error(`store_api_key 失败：${stored}`)
    const ready = await this.waitFor(
      `(() => { const el = document.querySelector('[contenteditable="true"]') || document.querySelector('textarea'); if (!el) return false; el.focus(); return true })()`,
      20_000,
      '输入框',
    ).catch(() => false)
    if (!ready) {
      // 首次启动可能停在空态或需要显式新建会话，Ctrl+N 兜底。
      await this.eval(
        `window.dispatchEvent(new KeyboardEvent('keydown', { key: 'n', code: 'KeyN', ctrlKey: true, bubbles: true, cancelable: true, composed: true }))`,
      )
      await this.waitFor(
        `(() => { const el = document.querySelector('[contenteditable="true"]') || document.querySelector('textarea'); if (!el) return false; el.focus(); return true })()`,
        15_000,
        '输入框（Ctrl+N 后）',
      )
    }
  }

  async sendPrompt(text) {
    await this.eval(
      `(() => { const el = document.querySelector('[contenteditable="true"]') || document.querySelector('textarea'); if (!el) return false; el.focus(); return true })()`,
    )
    await this.send('Input.insertText', { text })
    await sleep(350)
    await this.pressEnter()
    await sleep(600)
    // 斜杠命令可能先被补全菜单吃一次回车，没发出去就再按一次。
    if (text.startsWith('/')) {
      const stillThere = await this.eval(`(() => { const el = document.querySelector('[contenteditable="true"]'); return (el?.innerText || '').includes(${JSON.stringify(text)}) })()`)
      if (stillThere) {
        await this.pressEnter()
        await sleep(400)
      }
    }
  }

  async pressEnter() {
    const base = { key: 'Enter', code: 'Enter', windowsVirtualKeyCode: 13, nativeVirtualKeyCode: 13 }
    await this.send('Input.dispatchKeyEvent', { type: 'rawKeyDown', ...base })
    await this.send('Input.dispatchKeyEvent', { type: 'char', text: '\r', ...base })
    await this.send('Input.dispatchKeyEvent', { type: 'keyUp', ...base })
  }

  async openPriorSession(marker) {
    const deadline = Date.now() + 25_000
    while (Date.now() < deadline) {
      const body = await this.bodyText()
      if (body.includes(marker)) return true
      const clicked = await this.eval(`(() => {
        const rows = [...document.querySelectorAll('button,[role="treeitem"],li,a,div')]
          .filter((node) => {
            const text = (node.innerText || '').trim()
            return text.includes('N2N9 复验') && text.length < 60
          })
          .sort((a, b) => (a.innerText || '').length - (b.innerText || '').length)
        const row = rows[0]
        if (!row) return false
        ;(row.closest('[role="treeitem"],button,li,a') || row).click()
        return true
      })()`)
      if (clicked) {
        await sleep(1500)
        continue
      }
      await sleep(500)
    }
    return (await this.bodyText()).includes(marker)
  }

  async refreshSession(marker) {
    // 新建一个空会话再点回原会话：强制重新 follow 一次，拿到宿主运行态已知的快照。
    await this.eval(
      `window.dispatchEvent(new KeyboardEvent('keydown', { key: 'n', code: 'KeyN', ctrlKey: true, bubbles: true, cancelable: true, composed: true }))`,
    )
    await sleep(1500)
    const clicked = await this.eval(`(() => {
      const rows = [...document.querySelectorAll('button,[role="treeitem"],li,a,div')]
        .filter((node) => {
          const text = (node.innerText || '').trim()
          return text.includes('N2N9 复验') && text.length < 60
        })
        .sort((a, b) => (a.innerText || '').length - (b.innerText || '').length)
      const row = rows[0]
      if (!row) return false
      ;(row.closest('[role="treeitem"],button,li,a') || row).click()
      return true
    })()`)
    await sleep(2000)
    return clicked && (await this.bodyText()).includes(marker)
  }

  footer() {
    return this.bodyText().then((body) => {
      const matches = [...body.matchAll(/(\d+)\s*轮\s*(\d+)\s*步/g)]
      if (matches.length === 0) return null
      const last = matches[matches.length - 1]
      return [Number(last[1]), Number(last[2])]
    })
  }

  cardState(id) {
    return this.eval(`(() => {
      const root = document.querySelector('[data-chat-call-id="${id}"]')
      if (!root) return null
      const row = root.querySelector('[data-state]') || root
      return row.getAttribute('data-state')
    })()`)
  }

  async collectCards(ids) {
    const result = {}
    for (const id of ids) {
      result[id] = {
        state: await this.cardState(id),
        text: await this.expandCard(id),
      }
    }
    return result
  }

  // 历史回合有两级折叠：回合过程头（turn-process）与过程组头，正文落 `hidden`。
  // 取证前逐级展开（判据要的是「界面上有没有」，不是「不点开能不能看见」）。
  async expandProcessGroups() {
    const info = await this.eval(`(() => {
      const flow = document.querySelector('[data-chat-flow]') || document.body
      const rounds = []
      for (let round = 0; round < 4; round += 1) {
        const heads = [...flow.querySelectorAll('button[aria-expanded="false"]')]
        for (const head of heads) head.click()
        rounds.push({ round, clicked: heads.length, hiddenLeft: flow.querySelectorAll('[hidden]').length })
        if (heads.length === 0) break
      }
      return rounds
    })()`)
    await sleep(900)
    console.log(`[${scenarioId}] 展开：${JSON.stringify(info)}`)
    return info
  }

  async expandCard(id) {
    const before = await this.eval(`(() => {
      const root = document.querySelector('[data-chat-call-id="${id}"]')
      if (!root) return null
      root.scrollIntoView({ block: 'center' })
      return (root.innerText || '').slice(0, 40000)
    })()`)
    if (before === null) return null
    await this.eval(`(() => {
      const root = document.querySelector('[data-chat-call-id="${id}"]')
      const toggle = root?.querySelector('[data-expandable]') || root?.querySelector('button')
      if (toggle) toggle.click()
      return true
    })()`)
    await sleep(700)
    const after = await this.eval(`(() => {
      const root = document.querySelector('[data-chat-call-id="${id}"]')
      return root ? (root.innerText || '').slice(0, 40000) : null
    })()`)
    // 折叠/展开是同一个开关：点完反而变短就再点一次，恢复展开态。
    if (after !== null && after.length < before.length) {
      await this.eval(`(() => {
        const root = document.querySelector('[data-chat-call-id="${id}"]')
        const toggle = root?.querySelector('[data-expandable]') || root?.querySelector('button')
        if (toggle) toggle.click()
        return true
      })()`)
      await sleep(700)
      return this.eval(`(() => {
        const root = document.querySelector('[data-chat-call-id="${id}"]')
        return root ? (root.innerText || '').slice(0, 40000) : null
      })()`)
    }
    return after
  }

  turnEnds(file = this.rolloutPath()) {
    if (file === null) return []
    return readFileSync(file, 'utf8')
      .split('\n')
      .filter(Boolean)
      .map((line) => {
        try {
          return JSON.parse(line)
        } catch {
          return null
        }
      })
      .filter((item) => item?.type === 'turn_end')
  }

  rolloutPath() {
    const found = []
    const walk = (dir) => {
      if (!existsSync(dir)) return
      for (const entry of readdirSync(dir, { withFileTypes: true })) {
        const full = path.join(dir, entry.name)
        if (entry.isDirectory()) {
          walk(full)
        } else if (entry.name.endsWith('.jsonl')) {
          try {
            const content = readFileSync(full, 'utf8')
            if (!content.slice(0, 600).includes('session_meta')) continue
            // 有 turn_end 的才是跑过任务的会话；重开过程中新建的空会话不该抢走证据。
            found.push({
              full,
              score: content.includes('"type":"turn_end"') ? 1 : 0,
              mtime: statSync(full).mtimeMs,
            })
          } catch {
            // 读不了就跳过
          }
        }
      }
    }
    walk(dataDir)
    found.sort((a, b) => b.score - a.score || b.mtime - a.mtime)
    return found[0]?.full ?? null
  }

  extractSession(zipPath, dirName) {
    if (!existsSync(zipPath)) return null
    const target = path.join(scDir, dirName)
    rmSync(target, { recursive: true, force: true })
    mkdirSync(target, { recursive: true })
    const result = spawnSync(
      'powershell',
      ['-NoProfile', '-Command', `Expand-Archive -LiteralPath '${zipPath}' -DestinationPath '${target}' -Force`],
      { stdio: 'ignore' },
    )
    if (result.status !== 0) return null
    let file = null
    const walk = (dir) => {
      for (const entry of readdirSync(dir, { withFileTypes: true })) {
        const full = path.join(dir, entry.name)
        if (entry.isDirectory()) walk(full)
        else if (entry.name === 'session.v4.jsonl') file = full
      }
    }
    walk(target)
    if (file === null) return null
    const text = readFileSync(file, 'utf8')
    const entries = text
      .split('\n')
      .filter(Boolean)
      .map((line) => {
        try {
          return JSON.parse(line)
        } catch {
          return null
        }
      })
      .filter(Boolean)
    return { text, entries }
  }

  // 原生「另存为」不是 WebView 的 DOM，CDP 点不到；用 EnumWindows/EnumChildWindows 找到
  // 对话框，直接给文件名输入框发 WM_SETTEXT、给「保存」按钮发 BM_CLICK（不抢前台焦点，
  // 避免 SendKeys 误打进输入框）。脚本落证据目录，便于复核这次到底怎么点的。
  async exportZip(destPath) {
    if (existsSync(destPath)) rmSync(destPath)
    await this.sendPrompt('/export')
    const psPath = path.join(outRoot, `${scenarioId}-save-dialog.ps1`)
    const psLog = path.join(outRoot, `${scenarioId}-save-dialog.txt`)
    // PowerShell 5.1 按 ANSI 读无 BOM 的 .ps1：不写 BOM，脚本里的中文匹配串会变成乱码。
    writeFileSync(psPath, `\uFEFF${SAVE_DIALOG_SCRIPT}`, 'utf8')
    const out = openSync(psLog, 'w')
    const sender = spawn('powershell', ['-NoProfile', '-STA', '-File', psPath, '-Dest', destPath], {
      stdio: ['ignore', out, out],
    })
    const exited = new Promise((resolve) => sender.once('exit', resolve))
    const deadline = Date.now() + 40_000
    let size = 0
    while (Date.now() < deadline) {
      if (existsSync(destPath)) {
        const now = statSync(destPath).size
        if (now > 0 && now === size) break
        size = now
      }
      await sleep(600)
    }
    await Promise.race([exited, sleep(2000)])
    closeSync(out)
    if (!existsSync(destPath) || statSync(destPath).size === 0) {
      console.log(`[${scenarioId}] 导出未落盘，详见 ${path.basename(psLog)}`)
      return false
    }
    return true
  }

  async stop() {
    if (this.sock !== null) {
      try {
        this.sock.close()
      } catch {
        // 已断
      }
      this.sock = null
    }
    if (this.child !== null) {
      try {
        spawn('taskkill', ['/PID', String(this.child.pid), '/T', '/F'], { stdio: 'ignore' }).unref()
      } catch {
        // 可能已退出
      }
      this.child = null
    }
    await sleep(800)
  }
}

const driver = new Driver()
let exitCode = 1
try {
  await driver.launch()
  await driver.afterLaunch()
  await def.run(driver)
  exitCode = checks.every((item) => item.ok) ? 0 : 1
} catch (error) {
  check('执行未中断', false, String(error))
  await driver.shot(`${scenarioId}-failure`).catch(() => {})
  exitCode = 1
} finally {
  await driver.stop()
  await stub.close().catch(() => {})
  const rollout = driver.rolloutPath?.()
  if (rollout !== null && rollout !== undefined && !artifacts.includes(`${scenarioId}-rollout.jsonl`)) {
    try {
      copyFileSync(rollout, path.join(outRoot, `${scenarioId}-rollout.jsonl`))
      artifacts.push(`${scenarioId}-rollout.jsonl`)
    } catch {
      // 档案复制失败不影响判定
    }
  }
  writeFileSync(
    path.join(outRoot, `${scenarioId}-verdict.json`),
    JSON.stringify(
      {
        scenario: scenarioId,
        at: new Date().toISOString(),
        exe,
        exeSha256: createHash('sha256').update(readFileSync(exe)).digest('hex').toUpperCase(),
        checks,
        artifacts,
        stubState: stub.state,
      },
      null,
      2,
    ),
  )
  console.log(`[${scenarioId}] verdict → ${path.join(outRoot, `${scenarioId}-verdict.json`)}`)
}
process.exit(exitCode)
