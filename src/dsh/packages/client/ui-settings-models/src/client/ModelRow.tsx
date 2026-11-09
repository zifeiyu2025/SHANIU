/** Shared model fields and actions for both adapter catalog editors. */

import type { DragEvent, ReactNode } from 'react'
import {
  IconChevronDownOutlineRegular, IconChevronRightOutlineRegular, IconDragHandleOutlineRegular,
  IconTrashOutlineRegular, Tooltip, useRowDragPreview,
} from '@deepseek-ai/dsh-client-ui-primitives'
import type { DeepSeekModelDraft } from './DeepSeekModelsEditor.tsx'
import type { ModelsKey } from './locales.ts'
import { ModelInputTypes } from './ModelInputTypes.tsx'
import { ModelReasoningEfforts } from './ModelReasoningEfforts.tsx'
import type { ModelRowOrder } from './useModelRows.ts'
import styles from './ModelsSection.module.css'

/** A capacity's editable text and adapter-specific inherited hint. */
interface CapacityInput {
  value: string
  placeholder: string
  onChange: (value: string) => void
  onBlur?: () => void
}

/** Adapter-owned data and actions for one model row. */
interface ModelRowProps {
  model: DeepSeekModelDraft
  rowKey: number
  position: number
  order: ModelRowOrder
  inputField: 'inputModalities' | 'input'
  inputFallback?: readonly string[] | undefined
  inputLoading?: boolean
  /** Levels the row's `reasoningEfforts` may declare; absent or empty shows no reasoning control. */
  reasoningLevels?: readonly string[] | undefined
  expanded: boolean
  disabled: boolean
  t: (key: ModelsKey) => string
  contextWindow: CapacityInput
  maxTokens: CapacityInput
  onFieldChange: (field: 'id' | 'name', value: string | undefined) => void
  onIdBlur?: (value: string) => void
  onChange: (model: DeepSeekModelDraft) => void
  onToggle: () => void
  onRemove: () => void
}

function rowHalf(event: DragEvent<HTMLElement>): 'before' | 'after' {
  const rect = event.currentTarget.getBoundingClientRect()
  return event.clientY < rect.top + rect.height / 2 ? 'before' : 'after'
}

/** Known capability ids and the section copy that names them; `other` entries print their id as-is. */
const CAPABILITY_ITEMS = [
  ['vision', 'capVision'],
  ['context_window', 'capContextWindow'],
  ['max_output', 'capMaxTokens'],
  ['reasoning', 'capReasoningLevels'],
  ['tools', 'capTools'],
  ['builtin_web_search', 'capBuiltinWebSearch'],
] as const satisfies readonly (readonly [string, ModelsKey])[]

/** Source spellings to their badge copy; anything unrecognized reads as unknown. */
const CAPABILITY_SOURCES: Readonly<Record<string, ModelsKey>> = {
  manual: 'capSourceManual',
  endpoint: 'capSourceEndpoint',
  builtin: 'capSourceBuiltin',
  inferred: 'capSourceInferred',
  probe: 'capSourceProbe',
  unknown: 'capSourceUnknown',
}

/** Verification states to their copy; a manual value is never probed, so it shows none. */
const CAPABILITY_STATES: Readonly<Record<string, ModelsKey>> = {
  verified: 'capStateVerified',
  mismatch: 'capStateMismatch',
  unverified: 'capStateUnverified',
}

/** One capability view as a record, or undefined when the value is not an object. */
function viewOf(value: unknown): Record<string, unknown> | undefined {
  return typeof value === 'object' && value !== null && !Array.isArray(value) ? value as Record<string, unknown> : undefined
}

/** The row's `capabilities` map when it is an object; legacy and hostless rows carry none. */
function capabilityViews(model: DeepSeekModelDraft): Record<string, unknown> | undefined {
  return viewOf(model['capabilities'])
}

/** Display text of a capability value; `reasoning` lists its levels instead of a scalar. */
function capabilityValue(id: string, view: Record<string, unknown>, t: (key: ModelsKey) => string): string {
  if (id === 'reasoning') {
    const levels = Array.isArray(view['levels'])
      ? view['levels'].filter((level): level is string => typeof level === 'string')
      : []
    return levels.length > 0 ? levels.join('/') : '—'
  }
  const value = view['value']
  if (typeof value === 'boolean') return t(value ? 'capValueYes' : 'capValueNo')
  return typeof value === 'number' ? String(value) : '—'
}

/** Who said this value, with the catalog date when the value came from a dated table. */
function capabilitySource(view: Record<string, unknown>, t: (key: ModelsKey) => string): ReactNode {
  const source = typeof view['source'] === 'string' ? view['source'] : 'unknown'
  const updatedAt = typeof view['catalogUpdatedAt'] === 'string' ? view['catalogUpdatedAt'] : ''
  return (
    <span className={styles['capabilitySource']} data-source={source}>
      {t(CAPABILITY_SOURCES[source] ?? 'capSourceUnknown')}
      {updatedAt === '' ? null : ` · ${updatedAt}`}
    </span>
  )
}

/** Whether a probe confirmed the declaration; manual values have no probe to report. */
function capabilityState(view: Record<string, unknown>, t: (key: ModelsKey) => string): ReactNode {
  if (view['source'] === 'manual') return null
  const state = typeof view['state'] === 'string' ? view['state'] : ''
  const key = CAPABILITY_STATES[state]
  return key === undefined ? null : <span className={styles['capabilityState']} data-state={state}>{t(key)}</span>
}

/**
 * Render model fields with the display name in the expanded area of narrow rows.
 * @param props - drafted fields and their owning editor's actions.
 * @returns one expandable model entry.
 */
export function ModelRow(props: ModelRowProps): ReactNode {
  const { model, position, t, disabled } = props
  const preview = useRowDragPreview(styles['modelDragPreview'] as string)
  const capabilities = capabilityViews(model)
  const knownCapabilities = capabilities === undefined
    ? []
    : CAPABILITY_ITEMS.flatMap(([id, labelKey]) => {
      const view = viewOf(capabilities[id])
      return view === undefined ? [] : [[id, labelKey, view] as const]
    })
  const otherCapabilities = capabilities === undefined
    ? []
    : Object.entries(viewOf(capabilities['other']) ?? {})
      .flatMap(([id, raw]) => {
        const view = viewOf(raw)
        return view === undefined ? [] : [[id, view] as const]
      })
  const modelField = (field: 'id' | 'name') => (
    <label className={`${styles['modelField']} ${field === 'name' ? styles['modelName'] : ''}`}>
      {field === 'name'
        ? <span className={`${styles['modelFieldLabel']} ${styles['modelNameLabel']}`}>{t('modelName')}</span>
        : null}
      <input
        className={styles['input']}
        type="text"
        value={typeof model[field] === 'string' ? model[field] : ''}
        placeholder={t(field === 'id' ? 'modelId' : 'modelName')}
        aria-label={`${t(field === 'id' ? 'modelId' : 'modelName')} ${String(position)}`}
        disabled={disabled}
        onFocus={field === 'name' && !props.expanded ? props.onToggle : undefined}
        onChange={(event) => {
          const value = event.target.value
          props.onFieldChange(field, field === 'name' && value === '' ? undefined : value)
        }}
        onBlur={field === 'id' ? event => props.onIdBlur?.(event.target.value) : undefined}
      />
    </label>
  )
  return (
    <div
      className={styles['modelEntry']}
      data-row-key={props.rowKey}
      data-dragging={props.order.dragging || undefined}
      data-drop-target={props.order.dragging ? undefined : props.order.target}
      onDragOver={(event) => {
        if (!props.order.canDrop) return
        event.preventDefault()
        event.dataTransfer.dropEffect = 'move'
        props.order.onOver(rowHalf(event))
      }}
      onDragLeave={(event) => {
        if (event.relatedTarget instanceof Node && event.currentTarget.contains(event.relatedTarget)) return
        props.order.onLeave()
      }}
      onDrop={(event) => {
        if (!props.order.canDrop) return
        event.preventDefault()
        props.order.onDrop(rowHalf(event))
      }}
    >
      <div className={styles['modelRow']} data-expanded={props.expanded || undefined}>
        <Tooltip label={t('reorderModelHelp')} portal>
          <button
            type="button"
            className={`${styles['iconButton']} ${styles['modelDragHandle']}`}
            aria-label={`${t('reorderModel')} ${String(position)}`}
            aria-keyshortcuts="ArrowUp ArrowDown"
            aria-description={t('reorderModelHelp')}
            disabled={disabled}
            draggable={!disabled}
            onDragStart={(event) => {
              event.dataTransfer.effectAllowed = 'move'
              event.dataTransfer.setData('application/x-dsh-model-order', String(position))
              props.order.onStart()
              preview.start(event, event.currentTarget.closest('[data-row-key]') as HTMLElement)
            }}
            onDragEnd={() => { if (preview.clear()) props.order.onEnd() }}
            onKeyDown={(event) => {
              if (event.key !== 'ArrowUp' && event.key !== 'ArrowDown') return
              event.preventDefault()
              event.stopPropagation()
              props.order.onMove(event.key === 'ArrowUp' ? -1 : 1)
            }}
          >
            <IconDragHandleOutlineRegular size={14} />
          </button>
        </Tooltip>
        {modelField('id')}
        <button
          type="button"
          className={styles['iconButton']}
          aria-label={`${t('modelAdvanced')} ${String(position)}`}
          aria-expanded={props.expanded}
          title={t('modelAdvanced')}
          onClick={(event) => { event.currentTarget.focus(); props.onToggle() }}
        >
          {props.expanded ? <IconChevronDownOutlineRegular /> : <IconChevronRightOutlineRegular />}
        </button>
        <button
          type="button"
          className={`${styles['iconButton']} ${styles['iconButtonDanger']}`}
          aria-label={`${t('removeModel')} ${String(position)}`}
          title={t('removeModel')}
          disabled={disabled}
          onClick={props.onRemove}
        >
          <IconTrashOutlineRegular size={14} />
        </button>
        {modelField('name')}
        {props.expanded
          ? (
            <div className={styles['modelAdvanced']}>
              {(['contextWindow', 'maxTokens'] as const).map(field => (
                <label className={styles['modelField']} key={field}>
                  <span className={styles['modelFieldLabel']}>{t(field)}</span>
                  <input
                    className={styles['input']}
                    type="text"
                    inputMode="numeric"
                    value={props[field].value}
                    placeholder={props[field].placeholder}
                    aria-label={`${t(field)} ${String(position)}`}
                    disabled={disabled}
                    onChange={(event) => { props[field].onChange(event.target.value) }}
                    onBlur={props[field].onBlur}
                  />
                </label>
              ))}
              <ModelInputTypes
                model={model} field={props.inputField} position={position}
                fallback={props.inputFallback} disabled={disabled || props.inputLoading === true} t={t} onChange={props.onChange}
              />
              {knownCapabilities.length > 0
                ? (
                  <div className={styles['modelCapabilities']} role="group" aria-label={`${t('capSectionTitle')} ${String(position)}`}>
                    <span className={styles['modelFieldLabel']}>{t('capSectionTitle')}</span>
                    <ul className={styles['capabilityList']}>
                      {knownCapabilities.map(([id, labelKey, view]) => (
                        <li key={`known-${id}`} className={styles['capabilityChip']}>
                          <span className={styles['capabilityName']}>{t(labelKey)}</span>
                          <span className={styles['capabilityValue']}>{capabilityValue(id, view, t)}</span>
                          {capabilitySource(view, t)}
                          {capabilityState(view, t)}
                        </li>
                      ))}
                      {otherCapabilities.map(([id, view]) => (
                        <li key={`other-${id}`} className={styles['capabilityChip']}>
                          <span className={styles['capabilityName']}>{id}</span>
                          {capabilitySource(view, t)}
                        </li>
                      ))}
                    </ul>
                  </div>
                )
                : null}
              {props.reasoningLevels !== undefined && props.reasoningLevels.length > 0
                ? (
                  <ModelReasoningEfforts
                    model={model} levels={props.reasoningLevels} position={position} disabled={disabled} t={t} onChange={props.onChange}
                  />
                )
                : null}
            </div>
          )
          : null}
      </div>
    </div>
  )
}
