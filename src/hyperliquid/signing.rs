use alloy::{
    primitives::{Address, B256, keccak256},
    signers::{SignerSync, local::PrivateKeySigner},
    sol,
    sol_types::{SolStruct, eip712_domain},
};
use anyhow::Result;
use serde_json::{Value, json};
sol! { struct Agent { string source; bytes32 connectionId; } }
pub fn action_hash(
    action: &Value,
    vault: Option<Address>,
    nonce: u64,
    expires: Option<u64>,
) -> Result<B256> {
    // Preserve insertion order: Hyperliquid hashes MessagePack, not canonical JSON.
    let mut bytes = rmp_serde::to_vec_named(action)?;
    bytes.extend(nonce.to_be_bytes());
    if let Some(a) = vault {
        bytes.push(1);
        bytes.extend(a.as_slice());
    } else {
        bytes.push(0);
    }
    if let Some(t) = expires {
        bytes.push(0);
        bytes.extend(t.to_be_bytes());
    }
    Ok(keccak256(bytes))
}
pub fn sign(
    signer: &PrivateKeySigner,
    action: &Value,
    vault: Option<Address>,
    nonce: u64,
    expires: Option<u64>,
    mainnet: bool,
) -> Result<Value> {
    let agent = Agent {
        source: if mainnet { "a" } else { "b" }.into(),
        connectionId: action_hash(action, vault, nonce, expires)?,
    };
    let domain = eip712_domain! {name:"Exchange",version:"1",chain_id:1337,verifying_contract:Address::ZERO,};
    let sig = signer.sign_hash_sync(&agent.eip712_signing_hash(&domain))?;
    Ok(json!({"r":format!("{:#x}",sig.r()),"s":format!("{:#x}",sig.s()),"v":27+u8::from(sig.v())}))
}
