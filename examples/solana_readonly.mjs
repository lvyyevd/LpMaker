// Public RPC only. Builds UNSIGNED instructions using public deterministic fixture keys.
// No environment keys, wallet signatures or send methods are used.
import { createRequire } from "node:module";
import { writeFileSync } from "node:fs";
const require = createRequire(
  new URL("../adapters/solana/package.json", import.meta.url),
);
const sdk = require("@meteora-ag/dlmm");
const DLMM = sdk.default ?? sdk;
const {
  Connection,
  PublicKey,
  Keypair,
  Transaction,
  ComputeBudgetProgram,
} = require("@solana/web3.js");
const BN = require("bn.js");
const connection = new Connection(
  process.env.LPMAKER_SOL_PUBLIC_TEST_RPC ??
    "https://solana-rpc.publicnode.com",
  {
    commitment: "confirmed",
    disableRetryOnRateLimit: true,
    fetch: (u, i) => fetch(u, { ...i, signal: AbortSignal.timeout(20000) }),
  },
);
const pool = await DLMM.create(
  connection,
  new PublicKey("5rCf1DM8LjKTw4YqhnoLcngyZYeNnQqztScTogYHAS6"),
  { cluster: "mainnet-beta", skipSolWrappingOperation: true },
);
console.error("readonly: pool validated");
const a = await pool.getActiveBin();
const strategy = {
  minBinId: a.binId - 62,
  maxBinId: a.binId + 62,
  strategyType: sdk.StrategyType.Spot,
};
console.error("readonly: active bin read");
let quote;
let quoteStatus = "verified";
try {
  quote = pool.swapQuote(
    new BN(10000000),
    false,
    new BN(30),
    await pool.getBinArrayForSwap(false, 4),
  );
} catch {
  quoteStatus = "RPC blocked/incomplete; not verified";
}
const owner = Keypair.fromSeed(new Uint8Array(32).fill(7)).publicKey;
console.error("readonly: swap quote read");
let built;
let planStatus = "verified";
try {
  built = await pool.initializeMultiplePositionAndAddLiquidityByStrategy(
    async (n) =>
      Array.from({ length: n }, (_, i) =>
        Keypair.fromSeed(new Uint8Array(32).fill(8 + i)),
      ),
    new BN(350000000),
    new BN(40000000),
    strategy,
    owner,
    owner,
    0.3,
  );
} catch {
  planStatus = "RPC blocked/incomplete; not verified";
}
const records = (built?.instructionsByPositions ?? []).map((p) => ({
  unsigned_serialized_sizes: [
    [...p.initializeAtaIxs, p.initializePositionIx],
    ...p.addLiquidityIxs,
  ].map((ixs) => {
    const tx = new Transaction({
      feePayer: owner,
      recentBlockhash: owner.toBase58(),
    });
    tx.add(
      ComputeBudgetProgram.setComputeUnitLimit({ units: 1400000 }),
      ComputeBudgetProgram.setComputeUnitPrice({ microLamports: 10000 }),
      ...ixs.filter(
        (ix) => !ix.programId.equals(ComputeBudgetProgram.programId),
      ),
    );
    return tx.serialize({
      requireAllSignatures: false,
      verifySignatures: false,
    }).length;
  }),
  initialize_instructions: p.initializeAtaIxs.length + 1,
  liquidity_steps: p.addLiquidityIxs.map((xs) => xs.length),
  programs: [
    ...new Set(
      [
        p.initializePositionIx,
        ...p.initializeAtaIxs,
        ...p.addLiquidityIxs.flat(),
      ].map((ix) => ix.programId.toBase58()),
    ),
  ],
}));
const result = {
  observed_utc: new Date().toISOString(),
  pool: pool.pubkey.toBase58(),
  program: pool.program.programId.toBase58(),
  slot: await connection.getSlot("confirmed"),
  active_bin: a.binId,
  price_usdc: Number(a.pricePerToken),
  quote_status: quoteStatus,
  quote_10usdc_min_sol: quote?.minOutAmount.toString(),
  plan_status: planStatus,
  position_125bin_rent_sol:
    (await sdk.getPositionRentExemption(connection, new BN(125))) / 1e9,
  unsigned_plan: records,
  broadcasts: 0,
};
if (process.argv[2])
  writeFileSync(process.argv[2], JSON.stringify(result, null, 2));
console.log(JSON.stringify(result, null, 2));
