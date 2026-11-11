import { en } from './locales.ts'

/** Machine value of the preset that requires an explicit GUI risk gate. */
export const FULL_ACCESS_PRESET = 'danger-full-access'

/** Machine value of the experimental current-session review preset. */
export const AUTO_REVIEW_PRESET = 'auto'

/**
 * Machine value for "the current session's preset could not be read".
 *
 * Not a preset: it never appears in the catalog and the host never writes it.
 * Shanzhu's adapter emits it when the durable session record is unreadable, so
 * the picker can say "unreadable" instead of falling back to a concrete preset
 * (a fallback would render "unknown" as "read only", which the user then
 * confirms against a wrong premise). Keep in sync with
 * `src/transport/permission-presets.ts` (locked by a test there).
 */
export const PERMISSION_UNAVAILABLE = 'unavailable'

/** Locale dictionary key for a built-in permission preset label. */
export type PermissionPresetLabelKey =
  | 'preset.readOnly'
  | 'preset.workspaceWrite'
  | 'preset.fullAccess'
  | 'preset.unreadable'

const PRESET_LABEL_KEYS = new Map<string, PermissionPresetLabelKey>([
  ['read-only', 'preset.readOnly'],
  ['workspace-write', 'preset.workspaceWrite'],
  [FULL_ACCESS_PRESET, 'preset.fullAccess'],
  [PERMISSION_UNAVAILABLE, 'preset.unreadable'],
])

const DEFAULT_PRESET_LABELS: Record<PermissionPresetLabelKey, string> = {
  'preset.readOnly': en['preset.readOnly'],
  'preset.workspaceWrite': en['preset.workspaceWrite'],
  'preset.fullAccess': en['preset.fullAccess'],
  'preset.unreadable': en['preset.unreadable'],
}

/** Locale dictionary key for a built-in permission preset's one-line description. */
export type PermissionPresetDescriptionKey =
  | 'preset.readOnly.description'
  | 'preset.workspaceWrite.description'
  | 'preset.fullAccess.description'

const PRESET_DESCRIPTION_KEYS = new Map<string, PermissionPresetDescriptionKey>([
  ['read-only', 'preset.readOnly.description'],
  ['workspace-write', 'preset.workspaceWrite.description'],
  [FULL_ACCESS_PRESET, 'preset.fullAccess.description'],
])

const DEFAULT_PRESET_DESCRIPTIONS: Record<PermissionPresetDescriptionKey, string> = {
  'preset.readOnly.description': en['preset.readOnly.description'],
  'preset.workspaceWrite.description': en['preset.workspaceWrite.description'],
  'preset.fullAccess.description': en['preset.fullAccess.description'],
}

/**
 * Convert conventional kebab-case preset names into user-facing title case.
 * @param name - host-supplied preset label or key.
 * @returns the title-cased conventional key, or a non-kebab label unchanged.
 */
export function displayPresetName(name: string): string {
  if (!/^[a-z0-9]+(-[a-z0-9]+)*$/.test(name)) return name
  return name.split('-').map(word => word.charAt(0).toUpperCase() + word.slice(1)).join(' ')
}

/**
 * Render a permission preset under its product label.
 * @param value - preset machine value.
 * @param name - host-supplied preset name.
 * @param t - optional locale dictionary lookup for built-in product labels.
 * @returns the built-in product label or the conventional display name.
 */
export function displayPermissionPreset(
  value: string,
  name: string,
  t?: (key: PermissionPresetLabelKey) => string,
): string {
  const key = PRESET_LABEL_KEYS.get(value)
  if (key !== undefined && (name === value || name === DEFAULT_PRESET_LABELS[key])) {
    return t?.(key) ?? DEFAULT_PRESET_LABELS[key]
  }
  return displayPresetName(name)
}

/**
 * Render the one-line hint of a preset under its product copy.
 *
 * The hint is UI copy, so it comes from the dictionary rather than from the host
 * catalog: a host-supplied string would show Chinese inside an English UI.
 *
 * @param value - preset machine value.
 * @param t - locale dictionary lookup for built-in product copy.
 * @returns the localized hint, or undefined for presets the dictionary does not own.
 */
export function displayPermissionPresetDescription(
  value: string,
  t?: (key: PermissionPresetDescriptionKey) => string,
): string | undefined {
  const key = PRESET_DESCRIPTION_KEYS.get(value)
  if (key === undefined) return undefined
  return t?.(key) ?? DEFAULT_PRESET_DESCRIPTIONS[key]
}
