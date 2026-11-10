import type { ReactNode } from 'react'
import { SHANIU_BRAND_NAME } from './brand-name'
import './brand-name.css'

/**
 * `sidebar.brand.name` 的占用者。
 *
 * 侧栏把品牌名做成槽位，没有注册者时退回词典 `brand.localBuild`（中文即 "DSH 本地构建"）；
 * 上游由 ui-brand-official 填这个槽，而该包不在移植闭包内，所以这里由宿主自己填。
 *
 * 上游 wordmark 是 DeepSeek 的品牌字形（SVG），**不复制**；按 `SidebarRoot.module.css`
 * 的 `.brandName` 同规格排版（24px 盒高 / 18px / 600 / 0.04em 字距）排出 SHANIU。
 *
 * @returns 品牌名文本。
 */
export function ShaniuBrandName(): ReactNode {
  return <span className="shaniu-brand-name">{SHANIU_BRAND_NAME}</span>
}
