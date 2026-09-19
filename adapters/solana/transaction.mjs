import { createRequire } from "node:module";
const require = createRequire(import.meta.url);
const { Transaction, ComputeBudgetProgram } = require("@solana/web3.js");
const bs58 = require("bs58").default;
// Fixed priority-price ceiling + simulation. Actual fee is RPC-quoted before persistence.
export function signPrepared(
  transaction,
  ownerSigner,
  extraSigners,
  blockhashInfo,
  priority,
) {
  transaction.recentBlockhash = blockhashInfo.blockhash;
  transaction.feePayer = ownerSigner.publicKey;
  transaction.instructions = transaction.instructions.filter(
    (ix) => !ix.programId.equals(ComputeBudgetProgram.programId),
  );
  transaction.instructions.unshift(
    ComputeBudgetProgram.setComputeUnitLimit({ units: 1400000 }),
    ComputeBudgetProgram.setComputeUnitPrice({ microLamports: priority }),
  );
  const m = transaction.compileMessage();
  const required = m.accountKeys.slice(0, m.header.numRequiredSignatures);
  transaction.sign(
    ownerSigner,
    ...extraSigners.filter((s) => required.some((k) => k.equals(s.publicKey))),
  );
  const encoded = transaction.serialize();
  if (encoded.length > 1232) throw new Error("transaction too large");
  return {
    signature: bs58.encode(transaction.signature),
    raw_transaction: encoded.toString("base64"),
    blockhash: blockhashInfo.blockhash,
    last_valid_block_height: blockhashInfo.lastValidBlockHeight,
  };
}
export function validatedWire(raw, owner) {
  const tx = Transaction.from(Buffer.from(raw, "base64"));
  if (!tx.verifySignatures() || !tx.feePayer.equals(owner))
    throw new Error("invalid persisted transaction");
  return tx;
}
export async function incrementalRent(connection, before, after, keys, owner) {
  if (!Array.isArray(after) || after.length !== before.length)
    throw new Error("missing simulation accounts");
  let rent = 0;
  for (let i = 0; i < after.length; i++) {
    if (keys[i] === owner) continue;
    const oldSize = before[i]?.data.length ?? 0;
    const newSize = after[i]?.data?.[0]
      ? Buffer.from(after[i].data[0], "base64").length
      : 0;
    if (newSize > oldSize)
      rent += Math.max(
        0,
        (await connection.getMinimumBalanceForRentExemption(newSize)) -
          (before[i]?.lamports ?? 0),
      );
  }
  return rent;
}
