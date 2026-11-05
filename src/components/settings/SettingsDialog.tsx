import { useEffect, useState, type ReactNode } from 'react';
import type { ApprovalMode, SandboxMode, Transport } from '../../transport/types';
import {
  THEME_LABELS,
  THEME_PREFERENCES,
  applyPreference,
  readPreference,
  writePreference,
  type ThemePreference,
} from '../../theme';

/**
 * 沙箱三档。命名策略照 codex：机器名（`danger_full_access`）留在协议与配置文件里吓人，
 * UI label 用中性词（「完全访问」），后果由副文案承担——它的实测形态就是
 * `label: "Full Access"` 配 `description: "Use with caution: …"`（approval-presets/src/lib.rs:52-53）。
 */
const SANDBOX_OPTIONS: { value: SandboxMode; label: string; hint: string; danger?: boolean }[] = [
  { value: 'read_only', label: '只读', hint: 'Agent 只能读工作区文件，写入与命令都要审批。' },
  { value: 'workspace_write', label: '工作区可写', hint: '可改工作区内的文件并跑命令，越出工作区仍需审批。' },
  {
    value: 'danger_full_access',
    label: '完全访问',
    hint: '可不经审批修改本机任意文件、命令可访问网络。',
    danger: true,
  },
];

const APPROVAL_OPTIONS: { value: ApprovalMode; label: string; hint: string }[] = [
  { value: 'ask', label: '逐次询问', hint: '写入与命令每次都问。' },
  { value: 'auto_read_only', label: '只读自动放行', hint: '只读工具自动通过，其余仍逐次询问。' },
];

/**
 * 设置对话框（M1 最小集）：外观 / API Key（DPAPI 加密存储，留空不覆盖）/ 端点 / 模型 / 工作区。
 * 视觉对齐 dsh 菜单卡：r16、elevation-panel 描边、遮罩用 mask 令牌（dsh bg-mask-1 亮档 .24）。
 * 外观三档是纯前端显示偏好，保存即生效，不参与后端读写命令。
 */
export function SettingsDialog({
  transport,
  onClose,
}: {
  transport: Transport;
  onClose: () => void;
}) {
  const [baseUrl, setBaseUrl] = useState('');
  const [theme, setTheme] = useState<ThemePreference>(() => readPreference());
  const [model, setModel] = useState('');
  const [workspaceRoot, setWorkspaceRoot] = useState('');
  const [apiKey, setApiKey] = useState('');
  const [hasApiKey, setHasApiKey] = useState(false);
  const [sandboxMode, setSandboxMode] = useState<SandboxMode>('read_only');
  const [approvalMode, setApprovalMode] = useState<ApprovalMode>('ask');
  // 回显值单独记一份：危险档的确认只在"**进入**危险档"那一刻要，
  // 配置本来就是 danger_full_access 时不该拿它挡住一次无关的端点修改。
  const [savedSandbox, setSavedSandbox] = useState<SandboxMode>('read_only');
  const [dangerAcked, setDangerAcked] = useState(false);
  const [loaded, setLoaded] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const enteringDanger = sandboxMode === 'danger_full_access' && sandboxMode !== savedSandbox;

  useEffect(() => {
    let alive = true;
    transport
      .getSettings()
      .then((s) => {
        if (!alive) return;
        setBaseUrl(s.baseUrl);
        setModel(s.model);
        setWorkspaceRoot(s.workspaceRoot);
        setHasApiKey(s.hasApiKey);
        setSandboxMode(s.sandboxMode);
        setSavedSandbox(s.sandboxMode);
        setApprovalMode(s.approvalMode);
        setLoaded(true);
      })
      .catch((e: unknown) => alive && setError(String(e)));
    return () => {
      alive = false;
    };
  }, [transport]);

  // Esc 关设置卡。会话区的「Esc = 停止生成」看到 role=dialog 存在就让位（dsh 的
  // Escape 分层关浮层），所以这里只管关掉自己，不需要 stopPropagation。
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose();
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [onClose]);

  async function save() {
    setError(null);
    // 危险档的单点 gate，判据与 codex 一致（permission_popups.rs:340
    // `requires_confirmation = preset.id == "full-access"`）。
    // 未确认就**不发**保存请求，而不是提示一下照样存——后者等于确认只是装饰。
    if (enteringDanger && !dangerAcked) {
      setError('启用完全访问需要先勾选下方确认。');
      return;
    }
    try {
      await transport.saveSettings({
        baseUrl: baseUrl.trim(),
        model: model.trim(),
        workspaceRoot: workspaceRoot.trim(),
        apiKey: apiKey.trim() || undefined,
        sandboxMode,
        approvalMode,
      });
      onClose();
    } catch (e: unknown) {
      setError(String(e));
    }
  }

  return (
    <div
      className="fixed inset-0 z-40 flex items-center justify-center bg-mask"
      data-testid="settings-dialog"
      onMouseDown={(e) => {
        // 点遮罩关闭：只有落在遮罩本身才算"点外面"，冒泡自面板的不算
        if (e.target === e.currentTarget) onClose();
      }}
    >
      {/* role=dialog 同时是会话区 Esc 分层判据的依据：浮层开着时 Esc 归浮层，
          不该顺手把正在跑的任务取消掉（dsh keymap 的 Escape 分层关浮层同规则）。 */}
      <div
        role="dialog"
        aria-modal="true"
        aria-labelledby="settings-title"
        className="w-[460px] rounded-lg bg-layer1 p-5 shadow-panel"
      >
        <h2 id="settings-title" className="mb-4 text-[16px] font-medium leading-6 text-primary">
          设置
        </h2>

        {error && (
          <div role="alert" className="mb-3 rounded-md bg-danger/10 px-3 py-2 text-[13px] text-danger">
            {error}
          </div>
        )}

        <div className="flex flex-col gap-3 text-[14px] leading-[22px]">
          {/* 外观三档（SH-041）：纯显示偏好，存 localStorage，不参与后端读写命令。
              不用 Field——label 只能关联单个控件，包住 radiogroup 会污染第一个 radio 的名字。 */}
          <div className="flex flex-col gap-1.5">
            <span className="text-[13px] text-secondary">外观</span>
            <div role="radiogroup" aria-label="外观" className="flex gap-1" data-testid="settings-theme">
              {THEME_PREFERENCES.map((option) => (
                <button
                  key={option}
                  type="button"
                  role="radio"
                  aria-checked={theme === option}
                  onClick={() => {
                    writePreference(option);
                    applyPreference(option);
                    setTheme(option);
                  }}
                  className={`h-8 rounded-sm px-3 text-[13px] leading-5 transition-colors duration-100 ${
                    theme === option
                      ? 'bg-selector text-primary'
                      : 'text-secondary hover:bg-hover hover:text-primary'
                  }`}
                >
                  {THEME_LABELS[option]}
                </button>
              ))}
            </div>
          </div>
          <Field label={`API Key${hasApiKey ? '（已保存，留空不覆盖）' : ''}`}>
            <input
              type="password"
              value={apiKey}
              onChange={(e) => setApiKey(e.target.value)}
              placeholder={hasApiKey ? '••••••••' : 'sk-…'}
              disabled={!loaded}
              className={inputCls}
              data-testid="settings-api-key"
            />
          </Field>
          <Field label="Base URL">
            <input
              value={baseUrl}
              onChange={(e) => setBaseUrl(e.target.value)}
              placeholder="https://api.example.com/v1"
              disabled={!loaded}
              className={inputCls}
              data-testid="settings-base-url"
            />
          </Field>
          <Field label="模型">
            <input
              value={model}
              onChange={(e) => setModel(e.target.value)}
              placeholder="model-name"
              disabled={!loaded}
              className={inputCls}
              data-testid="settings-model"
            />
          </Field>
          <Field label="工作区目录">
            <input
              value={workspaceRoot}
              onChange={(e) => setWorkspaceRoot(e.target.value)}
              placeholder="D:\MyWorkspace\project"
              disabled={!loaded}
              className={inputCls}
              data-testid="settings-workspace"
            />
          </Field>

          {/* 沙箱与审批两档（SH-040）：值来自 getSettings 回显，保存即写进 config.toml。
              生效边界是任务级而不是运行中——AgentLoop 每次构造时才把两档快照进
              SessionPolicy（agent_loop/mod.rs:136-137），所以正在跑的那一轮不受影响。
              这条不是实现瑕疵，而是刻意保留的安全语义：任务中途放宽权限最危险。 */}
          <ModeGroup
            label="沙箱"
            testid="settings-sandbox"
            options={SANDBOX_OPTIONS}
            value={sandboxMode}
            disabled={!loaded}
            onChange={setSandboxMode}
          />
          {enteringDanger && (
            <div
              data-testid="danger-confirm"
              className="rounded-md border-[0.5px] border-danger/40 bg-danger/10 p-3"
            >
              <p className="text-[13px] leading-5 text-primary">
                启用完全访问后，Agent 可以不经审批修改工作区外的任意文件，并执行带网络访问的命令。
                <span className="text-danger">这会显著提高数据丢失、信息泄露或意外行为的风险。</span>
              </p>
              <label className="mt-2 flex items-start gap-2 text-[13px] leading-5 text-primary">
                <input
                  type="checkbox"
                  checked={dangerAcked}
                  onChange={(e) => setDangerAcked(e.target.checked)}
                  className="mt-1 accent-(--dsw-business-primary)"
                  data-testid="danger-ack"
                />
                我了解上述风险，仍要启用。改动从下一个任务开始生效，不影响正在运行的这一轮。
              </label>
            </div>
          )}
          <ModeGroup
            label="审批"
            testid="settings-approval"
            options={APPROVAL_OPTIONS}
            value={approvalMode}
            disabled={!loaded}
            onChange={setApprovalMode}
          />
        </div>

        <div className="mt-5 flex justify-end gap-2">
          <button type="button" onClick={onClose} className={ghostBtn}>
            取消
          </button>
          <button
            type="button"
            onClick={() => void save()}
            disabled={!loaded}
            className="flex h-9 items-center rounded-md bg-accent px-4 text-[14px] text-white transition-colors duration-100 hover:bg-accent-strong disabled:opacity-40"
          >
            保存
          </button>
        </div>
      </div>
    </div>
  );
}

const inputCls =
  'h-9 w-full rounded-md border-[0.5px] border-line3 bg-canvas px-3 text-[14px] text-primary outline-none placeholder:text-(--dsw-placeholder) focus:border-accent disabled:opacity-50';
const ghostBtn =
  'flex h-9 items-center rounded-md px-4 text-[14px] text-secondary transition-colors duration-100 hover:bg-hover hover:text-primary';

function Field({ label, children }: { label: string; children: ReactNode }) {
  return (
    <label className="flex flex-col gap-1.5">
      <span className="text-[13px] text-secondary">{label}</span>
      {children}
    </label>
  );
}

/**
 * 档位选择组。label 走中性词、后果走 hint，危险档的 hint 与选中态都用 danger 色——
 * 这是 codex 那套「机器名吓人、UI 名不吓人、副文案补吓人」分工的直接落地。
 */
function ModeGroup<T extends string>({
  label,
  testid,
  options,
  value,
  disabled,
  onChange,
}: {
  label: string;
  testid: string;
  options: readonly { value: T; label: string; hint: string; danger?: boolean }[];
  value: T;
  disabled: boolean;
  onChange: (value: T) => void;
}) {
  const current = options.find((option) => option.value === value);
  return (
    <div className="flex flex-col gap-1.5">
      {/* 与外观三档同理不用 Field 包：label 只能关联单个控件，
          包住 radiogroup 会污染第一个 radio 的可及名称。 */}
      <span className="text-[13px] text-secondary">{label}</span>
      <div role="radiogroup" aria-label={label} className="flex gap-1" data-testid={testid}>
        {options.map((option) => (
          <button
            key={option.value}
            type="button"
            role="radio"
            aria-checked={value === option.value}
            disabled={disabled}
            onClick={() => onChange(option.value)}
            className={`h-8 rounded-sm px-3 text-[13px] leading-5 transition-colors duration-100 disabled:opacity-50 ${
              value === option.value
                ? option.danger
                  ? 'bg-danger/15 text-danger'
                  : 'bg-selector text-primary'
                : 'text-secondary hover:bg-hover hover:text-primary'
            }`}
          >
            {option.label}
          </button>
        ))}
      </div>
      <span
        className={`text-[12px] leading-4 ${current?.danger ? 'text-danger' : 'text-caption'}`}
        data-testid={`${testid}-hint`}
      >
        {current?.hint}
      </span>
    </div>
  );
}
