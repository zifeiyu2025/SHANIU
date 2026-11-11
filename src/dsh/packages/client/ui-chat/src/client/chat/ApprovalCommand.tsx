/** Chat-owned approval detail resolving a correlated Tool call's arguments. */
import { useState, type ReactNode } from 'react'
import type { PropsRuntime } from '@deepseek-ai/dsh-client-ui-slots'
import type {} from '@deepseek-ai/dsh-client-ui-approval/client'
import type { ChatConversationViewNode, ChatNode } from '../contract/chat-nodes.ts'
import css from './ApprovalCommand.module.css'

interface ApprovalToolCall {
  readonly callId: string
  readonly argsRaw: string
}

/**
 * Extract a shell command from a correlated Tool call when its arguments carry one.
 * @param call - Tool call arguments, when a correlated call exists.
 * @returns command text, or undefined for absent, malformed, or unrelated arguments.
 */
export function commandOf(call: ApprovalToolCall | undefined): string | undefined {
  if (call === undefined) return undefined
  try {
    const args = JSON.parse(call.argsRaw) as Record<string, unknown>
    return typeof args.command === 'string' ? args.command : undefined
  } catch {
    return undefined
  }
}

type PatchOp = 'add' | 'update' | 'delete'

interface PatchFile {
  readonly path: string
  readonly op: PatchOp
  readonly added: number
  readonly removed: number
  readonly hunks: readonly string[]
}

const OP_LABEL: Record<PatchOp, string> = { add: '新增', update: '更新', delete: '删除' }

/**
 * Parse the V4A file sections of an `apply_patch` payload for the default (collapsed) view.
 *
 * This is presentation-only: it never decides what gets applied. Unknown or missing
 * headers simply yield fewer rows; the full patch is always reachable below.
 * @param patch - raw V4A patch text.
 * @returns one entry per `*** Add/Update/Delete File:` header.
 */
export function patchFilesOf(patch: string): PatchFile[] {
  const files: PatchFile[] = []
  let current: { path: string; op: PatchOp; added: number; removed: number; hunks: string[] } | undefined
  for (const line of patch.split('\n')) {
    const header = /^\*\*\* (Add|Update|Delete) File: (.+)$/.exec(line)
    if (header !== null) {
      const op: PatchOp = header[1] === 'Add' ? 'add' : header[1] === 'Update' ? 'update' : 'delete'
      current = { path: header[2]!.trim(), op, added: 0, removed: 0, hunks: [] }
      files.push(current)
      continue
    }
    if (current === undefined) continue
    if (line.startsWith('@@')) {
      current.hunks.push(line.trim().slice(0, 120))
      continue
    }
    if (line.startsWith('+') && !line.startsWith('+++')) current.added += 1
    else if (line.startsWith('-') && !line.startsWith('---')) current.removed += 1
  }
  return files
}

/** First `limit` lines, used only for the collapsed malformed preview. */
function previewLines(raw: string, limit: number): string {
  return raw.split('\n').slice(0, limit).join('\n')
}

function RawDetails({ raw, label }: { raw: string; label: string }): ReactNode {
  const [copied, setCopied] = useState(false)
  return (
    <details className={css.rawBlock}>
      <summary className={css.summary}>{label}</summary>
      <div className={css.rawHeader}>
        <button
          type="button"
          className={css.copy}
          onClick={() => {
            void navigator.clipboard?.writeText(raw).then(
              () => { setCopied(true) },
              () => { setCopied(false) },
            )
          }}
        >
          {copied ? '已复制' : '复制'}
        </button>
      </div>
      <pre className={css.raw}>{raw}</pre>
    </details>
  )
}

function PatchDetail({ files, raw }: { files: readonly PatchFile[]; raw: string }): ReactNode {
  return (
    <div className={css.detail}>
      <ul className={css.files}>
        {files.map((file, index) => (
          <li key={`${file.path}-${String(index)}`} className={css.file}>
            <div className={css.fileHead}>
              <code className={css.path}>{file.path}</code>
              <span className={css.op}>{OP_LABEL[file.op]}</span>
              {(file.added > 0 || file.removed > 0) && <span className={css.lines}>
                <span className={css.added}>+{file.added}</span>
                <span className={css.removed}>-{file.removed}</span>
              </span>}
            </div>
            {file.hunks.map((hunk, hunkIndex) => (
              <div key={`${String(hunkIndex)}`} className={css.hunk}>{hunk}</div>
            ))}
          </li>
        ))}
      </ul>
      <RawDetails raw={raw} label={`完整补丁（${String(raw.split('\n').length)} 行）`} />
    </div>
  )
}

function CommandDetail({ command, raw, cwd }: { command: string; raw: string; cwd?: string }): ReactNode {
  return (
    <div className={css.detail}>
      {/* 完整命令，不截断：换行与长度都是判据的一部分（AC2）。 */}
      <pre className={css.command}>{command}</pre>
      {cwd !== undefined && <div className={css.meta}>工作目录：{cwd}</div>}
      <RawDetails raw={raw} label="完整参数" />
    </div>
  )
}

function MessageDetail({ text, raw }: { text: string; raw: string }): ReactNode {
  const preview = previewLines(text, 5)
  return (
    <div className={css.detail}>
      <pre className={css.command}>{preview}{text.split('\n').length > 5 ? '\n…' : ''}</pre>
      <RawDetails raw={raw} label="完整参数 JSON" />
    </div>
  )
}

function formatValue(value: unknown): string {
  const text = typeof value === 'string' ? value : JSON.stringify(value)
  if (text === undefined) return String(value)
  return text.length > 120 ? `${text.slice(0, 120)}…` : text
}

function JsonDetail({ args, raw }: { args: Record<string, unknown>; raw: string }): ReactNode {
  const entries = Object.entries(args)
  return (
    <div className={css.detail}>
      {entries.length === 0
        ? <div className={css.meta}>（无参数）</div>
        : <dl className={css.kv}>
          {entries.map(([key, value]) => (
            <div key={key} className={css.kvRow}>
              <dt>{key}</dt>
              <dd>{formatValue(value)}</dd>
            </div>
          ))}
        </dl>}
      <RawDetails raw={raw} label="完整参数 JSON" />
    </div>
  )
}

function MalformedDetail({ raw, reason }: { raw: string; reason: string }): ReactNode {
  return (
    <div className={css.detail}>
      {/* fail-loud：解析不出来也要让用户看到原文，不能给一张空白卡。 */}
      <div className={css.warn}>参数无法解析：{reason}</div>
      <pre className={css.preview}>{previewLines(raw, 5)}</pre>
      <RawDetails raw={raw} label="原始参数" />
    </div>
  )
}

/** The raw arguments of the Tool call correlated with this approval, if still in the window. */
function correlatedArgs(
  nodes: Iterable<ChatConversationViewNode>, callId: string,
): { name: string; argsRaw: string } | undefined {
  for (const node of nodes) {
    if (node.kind !== 'tool-call') continue
    const root = (node as unknown as ChatNode<'tool-call'>).data.root
    if (root === undefined || root.callId !== callId) continue
    if ('kind' in root) {
      // 已结束的调用（提权询问常见）：paired call 仍在窗口内时保留原文。
      const argsRaw = root.call?.argsRaw
      return argsRaw === undefined ? undefined : { name: root.call?.name ?? '', argsRaw }
    }
    if (root.phase === 'start') return { name: root.name, argsRaw: root.argsRaw }
  }
  return undefined
}

/**
 * Render enough of the correlated Tool call for the user to judge what will happen:
 * patch file list, the complete command, or structured key/values — never a truncated prefix.
 * @param props - Approval identity and Session-standard Chat selector hook.
 * @returns structured detail, or null when no correlated call is available.
 */
export function ApprovalCommand({ callId, useChat }: PropsRuntime<'conversation.approval.detail'>) {
  // 两个选择器都只返回原始值：对象字面量每次都是新引用，会让 useSyncExternalStore 反复重渲染。
  const argsRaw = useChat((snapshot) => correlatedArgs(snapshot.nodes.values(), callId)?.argsRaw)
  const toolName = useChat((snapshot) => correlatedArgs(snapshot.nodes.values(), callId)?.name)
  if (argsRaw === undefined) return null
  let parsed: unknown
  try {
    parsed = JSON.parse(argsRaw)
  } catch {
    return <MalformedDetail raw={argsRaw} reason="参数不是合法 JSON" />
  }
  if (parsed === null || typeof parsed !== 'object' || Array.isArray(parsed)) {
    return <MalformedDetail raw={argsRaw} reason="参数不是对象" />
  }
  const fields = parsed as Record<string, unknown>
  if (toolName === 'apply_patch') {
    const patch = fields['patch']
    if (typeof patch !== 'string') {
      return <MalformedDetail raw={argsRaw} reason="apply_patch 缺少字符串 patch" />
    }
    const files = patchFilesOf(patch)
    if (files.length === 0) {
      return <MalformedDetail raw={patch} reason="补丁里没有 `*** Add/Update/Delete File:` 头" />
    }
    return <PatchDetail files={files} raw={patch} />
  }
  if (typeof fields['command'] === 'string') {
    const cwd = fields['cwd']
    return <CommandDetail command={fields['command']} raw={argsRaw} {...(typeof cwd === 'string' ? { cwd } : {})} />
  }
  const message = typeof fields['message'] === 'string'
    ? fields['message']
    : typeof fields['task'] === 'string' ? fields['task'] : undefined
  if (message !== undefined) return <MessageDetail text={message} raw={argsRaw} />
  return <JsonDetail args={fields} raw={argsRaw} />
}
