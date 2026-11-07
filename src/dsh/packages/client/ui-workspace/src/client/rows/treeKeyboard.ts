/**
 * The ARIA tree keyboard model shared by the grouped and flat Session lists.
 *
 * It is a module of its own (rather than a private helper beside the component) so the
 * behavior can be exercised without mounting the whole browser: the props the component
 * needs come from a dozen slots, while this handler only reads the DOM it is attached to.
 */
import type { KeyboardEvent } from 'react'

/**
 * Toggle a group one task after the keydown returns.
 *
 * Chromium implements the UIA `ExpandCollapsePattern` by focusing the row and dispatching a
 * real ArrowLeft/ArrowRight to it. Toggling synchronously inside that dispatch mutates the
 * accessibility tree while Chromium is still inside the platform action, which deadlocks the
 * WebView2 renderer (SH-101, measured: collapse of an empty group hung). One macrotask later
 * the dispatch has returned and the same toggle is safe.
 */
function toggleGroup(row: HTMLElement): void {
  setTimeout(() => {
    if (row.isConnected) row.click()
  }, 0)
}

/**
 * Up/Down/Home/End move focus between rows, Right expands a collapsed group (or enters it),
 * Left collapses an expanded group (or returns to the owning header), Enter toggles the
 * focused group.
 *
 * Every key the model owns is prevented. Without a handler the keys' default action inside
 * the sidebar's scroll containers kills the renderer (SH-101); preventing it is the crash
 * fix, and handling the keys is the tree behavior a `role="treeitem"` is expected to have.
 */
export function handleTreeKeyDown(event: KeyboardEvent<HTMLDivElement>): void {
  if (event.defaultPrevented || event.altKey || event.ctrlKey || event.metaKey) return
  const { key } = event
  if (key !== 'ArrowUp' && key !== 'ArrowDown' && key !== 'ArrowLeft' && key !== 'ArrowRight'
    && key !== 'Home' && key !== 'End' && key !== 'Enter') return
  const target = event.target
  if (!(target instanceof HTMLElement)) return
  const row = target.closest<HTMLElement>('[role="treeitem"]')
  if (row === null || !event.currentTarget.contains(row)) return
  const rows = [...event.currentTarget.querySelectorAll<HTMLElement>('[role="treeitem"]')]
  const index = rows.indexOf(row)
  if (index === -1) return
  event.preventDefault()
  const expanded = row.getAttribute('aria-expanded')
  switch (key) {
    case 'ArrowDown':
      rows[index + 1]?.focus()
      break
    case 'ArrowUp':
      rows[index - 1]?.focus()
      break
    case 'Home':
      rows[0]?.focus()
      break
    case 'End':
      rows[rows.length - 1]?.focus()
      break
    case 'ArrowRight':
      if (expanded === 'false') toggleGroup(row)
      else rows[index + 1]?.focus()
      break
    case 'ArrowLeft': {
      if (expanded === 'true') {
        toggleGroup(row)
        break
      }
      // A child row's owning header is the nearest expandable row above it.
      rows.slice(0, index).reverse().find(candidate => candidate.hasAttribute('aria-expanded'))?.focus()
      break
    }
    case 'Enter':
      if (expanded !== null) toggleGroup(row)
      break
  }
}
