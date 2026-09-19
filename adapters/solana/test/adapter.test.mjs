import test from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { EventEmitter } from "node:events";
import { POOL, binPrice, range, assertPool } from "../pool.mjs";
import {
  signPrepared,
  validatedWire,
  incrementalRent,
} from "../transaction.mjs";
import { GrpcMonitor } from "../grpc.mjs";
const require = createRequire(import.meta.url);
const {
  Keypair,
  Transaction,
  SystemProgram,
  PublicKey,
} = require("@solana/web3.js");
// Public deterministic fixture seeds; never load a user's environment key.
const signer = Keypair.fromSeed(new Uint8Array(32).fill(7));
const other = Keypair.fromSeed(new Uint8Array(32).fill(8));
test("SOL mint ordering and pool owner must match the pinned market", () => {
  const p = {
    pubkey: new PublicKey(POOL.address),
    tokenX: { publicKey: new PublicKey(POOL.base), mint: { decimals: 9 } },
    tokenY: { publicKey: new PublicKey(POOL.quote), mint: { decimals: 6 } },
    lbPair: { binStep: 4 },
  };
  assert.doesNotThrow(() => assertPool(p, POOL.program, POOL.genesis));
  p.lbPair.binStep = 10;
  assert.throws(() => assertPool(p, POOL.program, POOL.genesis));
});
test("bin range alignment uses 4bps geometric prices", () => {
  const r = range(-5600, 0.025);
  assert.equal(r.maxBinId - r.minBinId + 1, 125);
  assert.ok(binPrice(r.minBinId) < binPrice(-5600) / 1.025);
  assert.ok(binPrice(r.maxBinId) > binPrice(-5600) * 1.025);
  assert.throws(() => range(-5600, NaN));
});
test("persisted signed wire is stable and bound to its payer", () => {
  const tx = new Transaction().add(
    SystemProgram.transfer({
      fromPubkey: signer.publicKey,
      toPubkey: other.publicKey,
      lamports: 1,
    }),
  );
  const prepared = signPrepared(
    tx,
    signer,
    [],
    { blockhash: other.publicKey.toBase58(), lastValidBlockHeight: 100 },
    10000,
  );
  const decoded = validatedWire(prepared.raw_transaction, signer.publicKey);
  assert.equal(decoded.recentBlockhash, other.publicKey.toBase58());
  assert.equal(
    decoded.serialize().toString("base64"),
    prepared.raw_transaction,
  );
  assert.throws(() => validatedWire(prepared.raw_transaction, other.publicKey));
  const changed = Buffer.from(prepared.raw_transaction, "base64");
  changed[5] ^= 1;
  assert.throws(() =>
    validatedWire(changed.toString("base64"), signer.publicKey),
  );
});
test("rent counts creation and expansion, not ordinary token transfers or refunds", async () => {
  const conn = { getMinimumBalanceForRentExemption: async (n) => n * 10 };
  const before = [
    { data: Buffer.alloc(0), lamports: 1000 },
    null,
    { data: Buffer.alloc(10), lamports: 100 },
    { data: Buffer.alloc(10), lamports: 100 },
  ];
  const account = (n) => ({
    data: [Buffer.alloc(n).toString("base64"), "base64"],
    lamports: 999,
  });
  assert.equal(
    await incrementalRent(
      conn,
      before,
      [account(0), account(10), account(20), account(10)],
      ["payer", "new", "grown", "transfer"],
      "payer",
    ),
    200,
  );
  await assert.rejects(() => incrementalRent(conn, before, null, [], "payer"));
});
class Stream extends EventEmitter {
  constructor() {
    super();
    this.requests = [];
    this.destroyed = false;
  }
  write(r, cb) {
    this.requests.push(r);
    cb?.();
  }
  destroy() {
    if (!this.destroyed) {
      this.destroyed = true;
      this.emit("close");
    }
  }
}
let streams = [];
class FakeClient {
  async connect() {}
  async subscribe() {
    const s = new Stream();
    streams.push(s);
    return s;
  }
}
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
test("gRPC pong cannot mask 5min without pool data; reconnect and stop close old streams", async () => {
  streams = [];
  let now = 0;
  const m = new GrpcMonitor("https://unused", "test-only", "pool", 300, {
    Client: FakeClient,
    now: () => now,
    heartbeatMs: 5,
    reconnectMs: 5,
  });
  m.start();
  await sleep(15);
  assert.equal(m.state.generation, 1);
  assert.equal(streams[0].requests[0].commitment, 1);
  now = 299000;
  streams[0].emit("data", { pong: { id: 1 } });
  assert.equal(m.state.last_data_ms, 0);
  now = 301000;
  await sleep(35);
  assert.ok(m.state.generation >= 2);
  assert.equal(streams[0].destroyed, true);
  const latest = streams.at(-1);
  latest.emit("data", { account: { slot: "999" } });
  assert.equal(m.state.slot, 999);
  await m.stop();
  assert.equal(latest.destroyed, true);
  assert.equal(m.state.connected, false);
});
