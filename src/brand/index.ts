/**
 * SHANIU 自有品牌插件（宿主侧，非 vendored）：接管侧栏品牌名槽位。
 *
 * 槽位由 ui-sidebar 在它自己的 `register` 的 `children` 里声明，本插件用 `inject` 等它
 * 就位后填入占用者——与上游 ui-brand-official 的接缝一致，因此不改任何移植源码。
 * 品牌标记 `sidebar.brand.mark` 暂不接管：仓库里还没有自有 logo 资产，继续走上游 fallback。
 */
import type { Context } from '@deepseek-ai/cordis'
import type {} from '@deepseek-ai/dsh-client-ui-renderer/client'
import type {} from '@deepseek-ai/dsh-client-ui-sidebar/client'
import { ShaniuBrandName } from './BrandName'

/** 必需服务：UI 槽位注册表。 */
export const inject = ['slots']

/**
 * 把自有品牌名填进侧栏品牌槽位。
 * @param ctx - 客户端根上下文。
 */
export function apply(ctx: Context): void {
  ctx.slots.inject('sidebar.brand.name', () => ctx.slots.register({
    name: 'sidebar.brand.name',
  }, ShaniuBrandName))
}
