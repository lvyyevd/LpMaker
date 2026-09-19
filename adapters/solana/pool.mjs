// 池子白名单是独立模块；扩展新池需明确 mint、精度和对冲币种。
export const POOL = Object.freeze({
  address: "5rCf1DM8LjKTw4YqhnoLcngyZYeNnQqztScTogYHAS6",
  program: "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo",
  base: "So11111111111111111111111111111111111111112",
  quote: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
  baseDecimals: 9,
  quoteDecimals: 6,
  binStep: 4,
  genesis: "5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d",
});
export function assertPool(pool, accountOwner, genesis) {
  if (
    pool.pubkey.toBase58() !== POOL.address ||
    accountOwner !== POOL.program ||
    genesis !== POOL.genesis ||
    pool.tokenX.publicKey.toBase58() !== POOL.base ||
    pool.tokenY.publicKey.toBase58() !== POOL.quote ||
    pool.tokenX.mint.decimals !== 9 ||
    pool.tokenY.mint.decimals !== 6 ||
    pool.lbPair.binStep !== 4
  ) {
    throw new Error("Solana pool identity mismatch");
  }
}
export function binPrice(id) {
  return (1 + POOL.binStep / 10000) ** id * 1000;
}
export function range(active, width) {
  if (!(width > 0 && width < 0.25)) throw new Error("invalid DLMM range");
  const distance = Math.ceil(
    Math.log1p(width) / Math.log1p(POOL.binStep / 10000),
  );
  const lower = active - distance,
    upper = active + distance;
  if (upper - lower + 1 > 1400)
    throw new Error("DLMM range exceeds one extended position");
  return { minBinId: lower, maxBinId: upper };
}
