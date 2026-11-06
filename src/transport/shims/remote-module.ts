import type { TypertRemoteContribution } from '@deepseek-ai/dsh-typert-protocol'

export type RemoteCommands = Record<string, never>
export type RemoteEvents = Record<string, never>
const unavailable = Symbol('unavailable')

export const remote: TypertRemoteContribution = {
  get package(): string {
    throw new Error('shim(remote): 该命令面未接线，见 transport/stubs.ts 清单')
  },
  get descriptors(): TypertRemoteContribution['descriptors'] {
    throw new Error('shim(remote): 该命令面未接线，见 transport/stubs.ts 清单')
  },
}
export default remote
export { unavailable }
