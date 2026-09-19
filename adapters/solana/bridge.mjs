/** Only stdin/stdout JSON RPC, one request at a time. Never emit endpoints, keys or SDK error text.
 * Rust durably journals signed bytes BEFORE calling send. This process never auto-sends a plan.
 */
import { createRequire } from "node:module";
import { createInterface } from "node:readline";
import { POOL, assertPool, range, binPrice } from "./pool.mjs";
import { GrpcMonitor } from "./grpc.mjs";
import {
  signPrepared,
  validatedWire,
  incrementalRent,
} from "./transaction.mjs";
const require = createRequire(import.meta.url);
const sdk = require("@meteora-ag/dlmm");
const DLMM = sdk.default ?? sdk;
const {
  Connection,
  PublicKey,
  Keypair,
  Transaction,
  ComputeBudgetProgram,
} = require("@solana/web3.js");
const {
  getAssociatedTokenAddressSync,
  TOKEN_PROGRAM_ID,
} = require("@solana/spl-token");
const BN = require("bn.js");
const bs58 = require("bs58").default;
let config,
  connection,
  pool,
  owner,
  grpc,
  live = false,
  draft = null;
class ValidationError extends Error {}
const checked = (ok, message) => {
  if (!ok) throw new ValidationError(message);
};
const output = (value) => process.stdout.write(JSON.stringify(value) + "\n");
// Disable unsafe diagnostic emission from dependencies; application events use sanitized responses.
console.log = () => {};
console.warn = () => {};
console.error = () => {};
function key() {
  checked(live, "live not enabled");
  const raw = process.env[config.private_key_env];
  checked(raw, "missing signer");
  const bytes = raw.trim().startsWith("[")
    ? Uint8Array.from(JSON.parse(raw))
    : bs58.decode(raw.trim());
  const pair = Keypair.fromSecretKey(bytes);
  checked(pair.publicKey.equals(owner), "signer owner mismatch");
  return pair;
}
async function refresh() {
  // Rehydrate through the official factory: refetchStates also fetches disabled reward addresses,
  // which some RPC providers reject. This reads the pool/mints/reserves without those placeholders.
  pool = await DLMM.create(connection, new PublicKey(POOL.address), {
    cluster: "mainnet-beta",
    skipSolWrappingOperation: true,
  });
  const info = await connection.getAccountInfo(pool.pubkey, "confirmed");
  assertPool(pool, info?.owner.toBase58(), POOL.genesis);
}
async function balances() {
  if (!owner) return { base: 0, quote: 0, native: 0 };
  const atas = [POOL.base, POOL.quote].map((m) =>
    getAssociatedTokenAddressSync(new PublicKey(m), owner),
  );
  const infos = await connection.getMultipleAccountsInfo(atas, "confirmed");
  const values = infos.map((a, i) => {
    if (!a) return 0;
    checked(
      a.owner.equals(TOKEN_PROGRAM_ID) && a.data.length >= 165,
      "unexpected token account",
    );
    checked(
      new PublicKey(a.data.subarray(0, 32)).toBase58() ===
        [POOL.base, POOL.quote][i] &&
        new PublicKey(a.data.subarray(32, 64)).equals(owner),
      "ATA identity",
    );
    return Number(a.data.readBigUInt64LE(64)) / 10 ** [9, 6][i];
  });
  return {
    base: values[0],
    quote: values[1],
    native: (await connection.getBalance(owner, "confirmed")) / 1e9,
  };
}
async function observe() {
  await refresh();
  const slot = await connection.getSlot("confirmed");
  const block = await connection.getBlock(slot, {
    commitment: "confirmed",
    transactionDetails: "none",
    rewards: false,
    maxSupportedTransactionVersion: 0,
  });
  const blockTime = block?.blockTime;
  checked(blockTime, "missing block time");
  const active = await pool.getActiveBin();
  const rows = owner
    ? (await pool.getPositionsByUserAndLbPair(owner)).userPositions
    : [];
  const positions = rows.map((p) => {
    const d = p.positionData;
    checked(d.owner.equals(owner), "position owner mismatch");
    return {
      address: p.publicKey.toBase58(),
      lower_bin: d.lowerBinId,
      upper_bin: d.upperBinId,
      lower: binPrice(d.lowerBinId),
      upper: binPrice(d.upperBinId),
      base: Number(d.totalXAmount) / 1e9,
      quote: Number(d.totalYAmount) / 1e6,
      fee_base: Number(d.feeX.toString()) / 1e9,
      fee_quote: Number(d.feeY.toString()) / 1e6,
      revision: d.lastUpdatedAt.toString(),
    };
  });
  return {
    slot,
    block_hash: block.blockhash,
    time_ms: blockTime * 1000,
    observed_ms: Date.now(),
    active_bin: active.binId,
    bin_step: 4,
    price: Number(active.pricePerToken),
    positions,
    wallet: await balances(),
    grpc: grpc?.state ?? { connected: false },
    fee_pct: pool.getDynamicFee().toNumber(),
  };
}
async function allocation(value, width, bounds) {
  await refresh();
  const active = await pool.getActiveBin();
  const bins = bounds ?? range(active.binId, width);
  const unit = new BN(1e9);
  const y = sdk.autoFillYByStrategy(
    active.binId,
    4,
    unit,
    active.xAmount,
    active.yAmount,
    bins.minBinId,
    bins.maxBinId,
    sdk.StrategyType.Spot,
  );
  const quotePerSol = Number(y.toString()) / 1e6;
  checked(
    Number.isFinite(quotePerSol) && quotePerSol >= 0,
    "invalid auto-fill",
  );
  const x = value / (Number(active.pricePerToken) + quotePerSol);
  const positionRent = await sdk.getPositionRentExemption(
    connection,
    new BN(bins.maxBinId - bins.minBinId + 1),
  );
  return {
    ...bins,
    base: x,
    quote: x * quotePerSol,
    price: Number(active.pricePerToken),
    position_rent_sol: positionRent / 1e9,
  };
}
async function build(request) {
  checked(live && owner, "live owner required");
  checked(!draft, "unfinished local transaction plan");
  await refresh();
  const wallet = await balances();
  checked(wallet.native >= config.min_native_sol, "native gas reserve");
  const kind = request.kind;
  let transactions = [],
    signers = [],
    position = request.position ?? null,
    allocated;
  if (kind === "mint" || kind === "increase") {
    let bounds;
    if (kind === "increase") {
      const p = await pool.getPosition(new PublicKey(position));
      checked(p.positionData.owner.equals(owner), "position owner");
      bounds = {
        minBinId: p.positionData.lowerBinId,
        maxBinId: p.positionData.upperBinId,
      };
    }
    allocated = await allocation(request.value, request.width, bounds);
    const scale = Math.min(
      1,
      wallet.base / Math.max(allocated.base, 1e-15),
      wallet.quote / Math.max(allocated.quote, 1e-15),
    );
    checked(scale > 0.97, "insufficient inventory for DLMM allocation");
    const x = new BN(Math.floor(allocated.base * scale * 1e9));
    const y = new BN(Math.floor(allocated.quote * scale * 1e6));
    const strategy = {
      minBinId: allocated.minBinId,
      maxBinId: allocated.maxBinId,
      strategyType: sdk.StrategyType.Spot,
    };
    if (kind === "mint") {
      const built =
        await pool.initializeMultiplePositionAndAddLiquidityByStrategy(
          async (n) => {
            checked(n === 1, "only single extended position");
            return [Keypair.generate()];
          },
          x,
          y,
          strategy,
          owner,
          owner,
          config.slippage_bps / 100,
        );
      const p = built.instructionsByPositions[0];
      position = p.positionKeypair.publicKey.toBase58();
      signers = [p.positionKeypair];
      transactions = [
        new Transaction().add(...p.initializeAtaIxs, p.initializePositionIx),
        ...p.addLiquidityIxs.map((ix) => new Transaction().add(...ix)),
      ];
    } else
      transactions = await pool.addLiquidityByStrategyChunkable({
        positionPubKey: new PublicKey(position),
        totalXAmount: x,
        totalYAmount: y,
        strategy,
        user: owner,
        slippage: config.slippage_bps / 100,
      });
  } else if (kind === "remove") {
    const p = await pool.getPosition(new PublicKey(position));
    checked(p.positionData.owner.equals(owner), "position owner");
    transactions = await pool.removeLiquidity({
      user: owner,
      position: new PublicKey(position),
      fromBinId: p.positionData.lowerBinId,
      toBinId: p.positionData.upperBinId,
      bps: new BN(10000),
      shouldClaimAndClose: true,
      skipUnwrapSOL: true,
    });
  } else if (kind === "swap") {
    const sell = request.sell_base;
    const input = new BN(Math.floor(request.amount * 10 ** (sell ? 9 : 6)));
    checked(input.gtn(0), "zero swap");
    const arrays = await pool.getBinArrayForSwap(sell, 4);
    const q = pool.swapQuote(input, sell, new BN(config.slippage_bps), arrays);
    checked(q.consumedInAmount.eq(input), "partial quote");
    transactions = [
      await pool.swap({
        inToken: new PublicKey(sell ? POOL.base : POOL.quote),
        outToken: new PublicKey(sell ? POOL.quote : POOL.base),
        inAmount: input,
        minOutAmount: q.minOutAmount,
        lbPair: pool.pubkey,
        user: owner,
        binArraysPubkey: q.binArraysPubkey,
      }),
    ];
  } else throw new Error("unsupported operation");
  checked(transactions.length > 0, "empty plan");
  draft = {
    transactions,
    signers,
    position,
    kind,
    index: 0,
    remaining_rent: config.max_rent_sol * 1e9,
  };
  return { count: transactions.length, position, kind, allocated };
}
async function prepare(index) {
  checked(draft && index === draft.index, "plan sequence mismatch");
  const tx = draft.transactions[index];
  const bh = await connection.getLatestBlockhash("confirmed");
  const prepared = signPrepared(
    tx,
    key(),
    draft.signers,
    bh,
    config.priority_fee_microlamports,
  );
  const encoded = Buffer.from(prepared.raw_transaction, "base64");
  const fee = (
    await connection.getFeeForMessage(tx.compileMessage(), "confirmed")
  ).value;
  checked(
    fee !== null && fee <= config.max_transaction_fee_sol * 1e9,
    "fee ceiling",
  );
  // Rent is bounded from writable accounts actually created/resized by simulation.
  const accountKeys = tx.compileMessage().accountKeys.map((k) => k.toBase58());
  const result = await connection._rpcRequest("simulateTransaction", [
    encoded.toString("base64"),
    {
      encoding: "base64",
      sigVerify: true,
      commitment: "confirmed",
      accounts: { encoding: "base64", addresses: accountKeys },
    },
  ]);
  checked(
    !result.error && !result.result?.value?.err,
    "simulation account check failed",
  );
  const before = await connection.getMultipleAccountsInfo(
    accountKeys.map((k) => new PublicKey(k)),
    "confirmed",
  );
  const rent = await incrementalRent(
    connection,
    before,
    result.result.value.accounts,
    accountKeys,
    owner.toBase58(),
  );
  checked(rent <= draft.remaining_rent, "workflow rent ceiling");
  draft.remaining_rent -= rent;
  checked(
    (await balances()).native * 1e9 - fee - rent >= config.min_native_sol * 1e9,
    "gas reserve after rent",
  );
  checked(
    (await connection.getBlockHeight("confirmed")) + 10 <
      bh.lastValidBlockHeight,
    "blockhash nearly expired before preparation",
  );
  return {
    ...prepared,
    position: draft.position,
    kind: draft.kind,
    index,
    count: draft.transactions.length,
    estimated_fee_lamports: fee,
    rent_lamports: rent,
  };
}
async function handle(r) {
  if (r.method === "init") {
    checked(!config, "already initialized");
    config = r.config;
    live = r.live === true;
    checked(config.pool === POOL.address, "unsupported pool");
    owner = config.owner ? new PublicKey(config.owner) : null;
    const url = process.env[config.rpc_url_env];
    checked(url, "missing RPC URL");
    checked(/^https?:\/\//.test(url), "RPC protocol");
    connection = new Connection(url, {
      commitment: "confirmed",
      disableRetryOnRateLimit: true,
      fetch: async (url, init) =>
        fetch(url, { ...init, signal: AbortSignal.timeout(20000) }),
    });
    pool = await DLMM.create(connection, new PublicKey(POOL.address), {
      cluster: "mainnet-beta",
      skipSolWrappingOperation: true,
    });
    const info = await connection.getAccountInfo(pool.pubkey, "confirmed");
    assertPool(pool, info?.owner.toBase58(), await connection.getGenesisHash());
    const endpoint = process.env[config.grpc_url_env];
    const token = process.env[config.grpc_token_env];
    if (endpoint) {
      checked(/^https?:\/\//.test(endpoint), "gRPC protocol");
      grpc = new GrpcMonitor(
        endpoint,
        token,
        POOL.address,
        config.idle_timeout_seconds,
      );
      grpc.start();
    } else checked(!config.require_grpc, "missing gRPC endpoint");
    return { pool: POOL, live };
  }
  checked(config, "init first");
  if (r.method === "observe") return observe();
  if (r.method === "allocation") return allocation(r.value, r.width, r.bounds);
  if (r.method === "plan") return build(r);
  if (r.method === "prepare") return prepare(r.index);
  if (r.method === "send") {
    checked(live, "live not enabled");
    const tx = validatedWire(r.raw_transaction, owner);
    return {
      signature: await connection.sendRawTransaction(tx.serialize(), {
        skipPreflight: false,
        maxRetries: 0,
        preflightCommitment: "confirmed",
      }),
    };
  }
  if (r.method === "ack") {
    checked(draft && r.index === draft.index, "ack sequence");
    draft.index++;
    if (draft.index === draft.transactions.length) draft = null;
    return {};
  }
  if (r.method === "status") {
    const status = (
      await connection.getSignatureStatuses([r.signature], {
        searchTransactionHistory: true,
      })
    ).value[0];
    return {
      status,
      finalized_height: await connection.getBlockHeight("finalized"),
    };
  }
  throw new Error("unknown method");
}
const reader = createInterface({ input: process.stdin, crlfDelay: Infinity });
for await (const line of reader) {
  let request;
  try {
    checked(line.length < 2_000_000, "request too large");
    request = JSON.parse(line);
    const value = await handle(request);
    output({ id: request.id, ok: true, value });
  } catch (e) {
    output({
      id: request?.id ?? null,
      ok: false,
      error:
        e instanceof ValidationError
          ? e.message
          : "SDK/RPC operation failed (credentials redacted)",
      error_type: e instanceof TypeError ? "type_error" : "operation_failed",
      method: request?.method ?? "decode",
    });
  }
}
await grpc?.stop();
