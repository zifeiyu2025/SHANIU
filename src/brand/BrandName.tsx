import type { ReactNode } from 'react'
import { SHANIU_BRAND_NAME } from './brand-name'

/**
 * `sidebar.brand.name` 的占用者。
 *
 * 侧栏把品牌名做成槽位，没有注册者时退回词典 `brand.localBuild`（中文即 "DSH 本地构建"）；
 * 上游由 ui-brand-official 填这个槽，而该包不在移植闭包内，所以这里由宿主自己填。
 * 契约是 `children?: never`：占用者自带内容与宽度，父级 `.brandName` 已给好字号与行高，
 * 因此不再叠加排版声明，只输出品牌名本身。
 *
 * @returns 品牌名文本。
 */
export function ShaniuBrandName(): ReactNode {
  return <span>{SHANIU_BRAND_NAME}</span>
}
