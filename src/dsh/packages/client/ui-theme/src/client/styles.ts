import type { Context } from '@deepseek-ai/cordis'
import base from '../styles/base.css?inline'
import cornerShape from '../styles/corner-shape.css?inline'
import designPlatform from '../styles/design-platform.css?inline'
import focus from '../styles/focus.css?inline'
import onboarding from '../styles/onboarding.css?inline'
import scrollbar from '../styles/scrollbar.css?inline'
import gradientShadowText from '../styles/gradient-shadow-text.css?inline'
import shiki from '../styles/shiki.css?inline'

const HAIRLINE_WIDTH = '--dsh-hairline-width'

const STYLES = [
  ['base.css', base],
  ['corner-shape.css', cornerShape],
  ['design-platform.css', designPlatform],
  ['focus.css', focus],
  ['onboarding.css', onboarding],
  ['scrollbar.css', scrollbar],
  ['gradient-shadow-text.css', gradientShadowText],
  ['shiki.css', shiki],
] as const

/**
 * Mount global theme sheets; window-backed documents also publish the DPR-aware stroke width.
 * @param ctx - Owning plugin context.
 */
export function installThemeStyles(ctx: Context): void {
  if (typeof document === 'undefined') return
  const sheets = themeStyleSheets()
  const view = document.defaultView
  if (view !== null) ctx.effect(() => {
    const root = document.documentElement
    const previousWidth = root.style.getPropertyValue(HAIRLINE_WIDTH)
    const previousPriority = root.style.getPropertyPriority(HAIRLINE_WIDTH)
    const matchMedia = typeof view.matchMedia === 'function' ? view.matchMedia.bind(view) : undefined
    let resolution: MediaQueryList | undefined
    const updateStroke = (): void => {
      // Positive borders round down to whole device pixels, with a one-pixel minimum.
      const dpr = view.devicePixelRatio
      const width = Math.max(1, Math.floor(0.5 * dpr)) / dpr
      root.style.setProperty(HAIRLINE_WIDTH, `${width}px`)
      resolution?.removeEventListener('change', updateStroke)
      resolution = matchMedia?.(`(resolution: ${dpr}dppx)`)
      resolution?.addEventListener('change', updateStroke)
    }
    updateStroke()
    return () => {
      resolution?.removeEventListener('change', updateStroke)
      if (previousWidth === '') root.style.removeProperty(HAIRLINE_WIDTH)
      else root.style.setProperty(HAIRLINE_WIDTH, previousWidth, previousPriority)
    }
  }, 'ui-theme: DPR-aware hairline width')
  for (const { name, sheet } of sheets) {
    ctx.effect(() => {
      // 浏览器里该列表恒存在（空也是空数组）；jsdom 的 document 在首次赋值前没有这个属性，
      // 断言对象是它，故这里按可缺省读一次，避免用例里先赋一次空数组来迁就实现。
      const current = document.adoptedStyleSheets as readonly CSSStyleSheet[] | undefined
      document.adoptedStyleSheets = [...(current ?? []), sheet]
      return () => {
        document.adoptedStyleSheets = document.adoptedStyleSheets.filter((adopted) => adopted !== sheet)
      }
    }, `ui-theme: ${name} stylesheet`)
  }
}

export interface ThemeStyleSheet {
  /** 样式表文件名（`base.css` 等），仅用于诊断与用例断言。 */
  name: string
  sheet: CSSStyleSheet
}

let cachedSheets: ThemeStyleSheet[] | undefined

/**
 * 主题样式表，按依赖顺序（`base.css` 在前）。
 *
 * 用构造式样式表挂载而不用 `<style>` 标签：打包构建下宿主 CSP 的 `style-src` 只放行
 * 静态资源里带 nonce 的标签，运行时 `document.createElement('style')` 会被整表拦掉
 * （H2 实测：8 张表全灭，主题色与布局丢失、界面近乎全黑）。构造式样式表不经过
 * HTML/CSS 解析器，不受 `style-src` 约束，也不需要放宽 `'unsafe-inline'`。
 *
 * 构造式样式表不进 DOM，没有节点可查，故用例按引用断言这里的顺序与归属。
 */
export function themeStyleSheets(): ThemeStyleSheet[] {
  cachedSheets ??= STYLES.map(([name, css]) => {
    const sheet = new CSSStyleSheet()
    sheet.replaceSync(css)
    return { name, sheet }
  })
  return cachedSheets
}
