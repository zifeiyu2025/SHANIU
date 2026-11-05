/**
 * 紧凑 token 计数（对齐 dsh token-format.ts formatTokens）：
 * 517 / 12.2K / 517K / 1.2M。
 */
export function formatTokens(value: number): string {
  if (value < 0 || !Number.isFinite(value)) return '0';
  if (value < 1_000) return String(Math.round(value));
  const scaled = (candidate: number): string =>
    candidate >= 100 ? String(Math.round(candidate)) : String(Math.round(candidate * 10) / 10);
  if (value < 1_000_000) return `${scaled(value / 1_000)}K`;
  return `${scaled(value / 1_000_000)}M`;
}
