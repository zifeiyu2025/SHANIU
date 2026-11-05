import { describe, expect, it } from 'vitest';
import { formatTokens } from '../utils/token-format';

describe('formatTokens（对齐 dsh token-format）', () => {
  it('千以下原样', () => {
    expect(formatTokens(0)).toBe('0');
    expect(formatTokens(517)).toBe('517');
    expect(formatTokens(999)).toBe('999');
  });

  it('千到百万为 K（一位小数，≥100 收整数）', () => {
    expect(formatTokens(1_000)).toBe('1K');
    expect(formatTokens(12_200)).toBe('12.2K');
    expect(formatTokens(517_000)).toBe('517K');
  });

  it('百万以上为 M', () => {
    expect(formatTokens(1_200_000)).toBe('1.2M');
    expect(formatTokens(12_000_000)).toBe('12M');
  });
});
