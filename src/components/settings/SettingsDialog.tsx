import { useEffect, useState, type ReactNode } from 'react';
import type { Transport } from '../../transport/types';
import {
  THEME_LABELS,
  THEME_PREFERENCES,
  applyPreference,
  readPreference,
  writePreference,
  type ThemePreference,
} from '../../theme';

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
  const [loaded, setLoaded] = useState(false);
  const [error, setError] = useState<string | null>(null);

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
        setLoaded(true);
      })
      .catch((e: unknown) => alive && setError(String(e)));
    return () => {
      alive = false;
    };
  }, [transport]);

  async function save() {
    setError(null);
    try {
      await transport.saveSettings({
        baseUrl: baseUrl.trim(),
        model: model.trim(),
        workspaceRoot: workspaceRoot.trim(),
        apiKey: apiKey.trim() || undefined,
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
    >
      <div className="w-[460px] rounded-lg bg-layer1 p-5 shadow-panel">
        <h2 className="mb-4 text-[16px] font-medium leading-6 text-primary">设置</h2>

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
