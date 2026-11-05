/**
 * 传输面 shim：替换 DSH 的 typert 生成物（`lib/typert.remote-client.*`）。
 * DSH 的 `/remote` 子路径是 typert generator 从宿主服务生成的 RPC 代理；
 * SHANIU 的宿主是 Rust 后端 + EventMsg 协议，该生成物整体由本适配层取代。
 * 本文件提供类型兜底（全部标 never/unknown），真实能力按 stubs.ts 清单逐项接线。
 */

/** 任意包 `/remote` 子路径的兜底导出：命令面未接线，一律 unknown。 */
export type RemoteCommands = Record<string, never>
export type RemoteEvents = Record<string, never>
declare const unavailable: unique symbol
/** 运行时占位：被 value import 时给出带标记的空对象，调用即抛。 */
export const remote: unknown = new Proxy({}, {
  get: () => () => { throw new Error('shim(remote): 该命令面未接线，见 transport/stubs.ts 清单') },
})
export default remote
export { unavailable }
