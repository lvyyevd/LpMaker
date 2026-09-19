use super::*;

fn decode(candidate: &Value) -> Result<TxEnvelope> {
    let raw = hex::decode(
        candidate["raw_transaction"]
            .as_str()
            .context("missing signed transaction")?
            .trim_start_matches("0x"),
    )?;
    ensure!(
        candidate["hash"]
            .as_str()
            .is_some_and(|h| h.eq_ignore_ascii_case(&format!("{:#x}", keccak256(&raw)))),
        "stored signed transaction hash mismatch"
    );
    let mut bytes = raw.as_slice();
    let tx = TxEnvelope::decode_2718(&mut bytes)?;
    ensure!(bytes.is_empty(), "trailing signed transaction bytes");
    Ok(tx)
}

impl Executor {
    // A receipt hash alone is insufficient if persisted recovery data is corrupt.
    // Verify that every candidate really was signed by this wallet for this nonce
    // and the same call before accepting any candidate's receipt.
    pub(super) fn validated_pending_hashes(&self, pending: &Value) -> Result<Vec<String>> {
        ensure!(pending["venue"] == "evm", "pending operation is not EVM");
        ensure!(
            pending["owner"]
                .as_str()
                .is_some_and(|o| o.eq_ignore_ascii_case(&self.owner().to_string())),
            "pending owner mismatch"
        );
        let root = decode(pending)?;
        let nonce = pending["nonce"].as_u64().context("pending nonce")?;
        ensure!(
            root.chain_id() == Some(self.venue.cfg.chain_id)
                && root.nonce() == nonce
                && root.recover_signer()? == self.owner(),
            "signed pending chain/nonce/signer mismatch"
        );
        let hashes = super::super::nonce::hashes(pending)?;
        if let Some(replacements) = pending.get("replacements") {
            for candidate in replacements.as_array().context("invalid replacements")? {
                let tx = decode(candidate)?;
                ensure!(
                    tx.chain_id() == root.chain_id()
                        && tx.nonce() == nonce
                        && tx.recover_signer()? == self.owner(),
                    "replacement chain/nonce/signer mismatch"
                );
                ensure!(
                    tx.to() == root.to()
                        && tx.input() == root.input()
                        && tx.value() == root.value()
                        && tx.gas_limit() == root.gas_limit(),
                    "replacement call differs from original"
                );
            }
        }
        Ok(hashes)
    }

    /// Explicit recovery only: identical ERC20 approval and nonce, higher bounded fees.
    /// Never replay price-sensitive mint/swap operations using an old quote.
    pub async fn retry_approval(&self, expected_hash: &str) -> Result<Value> {
        let _guard = self.send_lock.lock().await;
        let mut pending = self.store.pending()?.context("no pending EVM approval")?;
        ensure!(pending["venue"] == "evm", "pending operation is not EVM");
        ensure!(
            pending["hash"]
                .as_str()
                .is_some_and(|h| h.eq_ignore_ascii_case(expected_hash)),
            "expected original hash differs from pending operation"
        );
        ensure!(
            pending["owner"]
                .as_str()
                .is_some_and(|o| o.eq_ignore_ascii_case(&self.owner().to_string())),
            "pending owner mismatch"
        );
        let op = pending["operation"].clone();
        ensure!(
            matches!(op["kind"].as_str(), Some("approve" | "approve_zero")),
            "fee recovery only supports ERC20 approve; retain other operations for reconciliation"
        );
        let token: Address = op["token"].as_str().context("approval token")?.parse()?;
        let spender: Address = op["spender"]
            .as_str()
            .context("approval spender")?
            .parse()?;
        let amount =
            U256::from_str_radix(op["raw_amount"].as_str().context("approval amount")?, 10)?;
        ensure!(
            [self.venue.base, self.venue.quote].contains(&token),
            "approval token not allowlisted"
        );
        ensure!(
            [self.venue.manager, self.venue.router].contains(&spender),
            "approval spender not allowlisted"
        );
        ensure!(
            op["kind"] != "approve_zero" || amount == U256::ZERO,
            "approve_zero amount mismatch"
        );
        let data = IERC20::approveCall {
            spender,
            value: amount,
        }
        .abi_encode();
        let original = decode(&pending)?;
        let nonce = pending["nonce"].as_u64().context("pending nonce")?;
        let validate = |tx: &TxEnvelope| -> Result<()> {
            ensure!(
                matches!(tx, TxEnvelope::Legacy(_) | TxEnvelope::Eip1559(_)),
                "unsupported recovery transaction type"
            );
            ensure!(
                tx.chain_id() == Some(self.venue.cfg.chain_id) && tx.nonce() == nonce,
                "signed approval chain/nonce mismatch"
            );
            ensure!(
                tx.recover_signer()? == self.owner(),
                "signed approval signer mismatch"
            );
            ensure!(
                tx.to() == Some(token)
                    && tx.value() == U256::ZERO
                    && tx.input().as_ref() == data.as_slice(),
                "signed approval differs from recorded operation"
            );
            ensure!(
                tx.gas_limit() == original.gas_limit(),
                "replacement gas limit mismatch"
            );
            Ok(())
        };
        validate(&original)?;
        let hashes = super::super::nonce::hashes(&pending)?;
        let mut latest = original.clone();
        if let Some(replacements) = pending.get("replacements") {
            for candidate in replacements.as_array().context("invalid replacements")? {
                latest = decode(candidate)?;
                validate(&latest)?;
            }
        }
        let rpc = &self.venue.rpc;
        ensure!(
            hex_u64(&rpc.request("eth_chainId", json!([])).await?)? == self.venue.cfg.chain_id,
            "recovery chain mismatch"
        );
        // Check every historical hash before signing. A prior attempt may have mined.
        let mut known_transaction = false;
        for hash in &hashes {
            if !rpc
                .request("eth_getTransactionReceipt", json!([hash]))
                .await?
                .is_null()
            {
                return self.wait_receipt(expected_hash, &op).await;
            }
            let tx = rpc
                .request("eth_getTransactionByHash", json!([hash]))
                .await?;
            if !tx.is_null() {
                ensure!(
                    tx["hash"]
                        .as_str()
                        .is_some_and(|h| h.eq_ignore_ascii_case(hash)),
                    "transaction lookup hash mismatch"
                );
                known_transaction = true;
            }
        }
        let state = self.nonce.refresh().await?;
        ensure!(
            state.latest == nonce,
            "approval nonce already consumed or RPC behind; retain pending and reconcile receipts"
        );
        let next = nonce.checked_add(1).context("nonce overflow")?;
        ensure!(
            state.next_floor == next
                && state
                    .inflight
                    .as_ref()
                    .is_some_and(|p| p["hash"] == pending["hash"] && p["nonce"] == nonce),
            "nonce reservation mismatch"
        );
        ensure!(
            state.pending == nonce || (known_transaction && state.pending == next),
            "unknown external pending transaction; no fee replacement"
        );
        ensure!(
            hashes.len() <= 8,
            "approval fee replacement limit reached; retain state for manual review"
        );
        let call = json!({"from":self.owner(),"to":token,"data":format!("0x{}",hex::encode(&data)),"value":"0x0"});
        rpc.request("eth_call", json!([call, "latest"]))
            .await
            .context("approval recovery simulation failed")?;
        let estimated = hex_u64(&rpc.request("eth_estimateGas", json!([call])).await?)?;
        ensure!(
            estimated <= original.gas_limit(),
            "original approval gas limit no longer sufficient"
        );
        let mut fees = Fees::estimate(rpc, self.venue.cfg.gas_fee_buffer_bps).await?;
        fees.replacement(
            latest.max_fee_per_gas(),
            latest
                .max_priority_fee_per_gas()
                .unwrap_or(latest.max_fee_per_gas()),
        )?;
        self.check_gas_budget(original.gas_limit(), &fees, original.input().len())
            .await?;
        let raw = self.sign_transaction(nonce, original.gas_limit(), token, data, &fees)?;
        let hash = format!("{:#x}", keccak256(&raw));
        ensure!(
            !hashes.contains(&hash),
            "replacement did not change transaction hash"
        );
        let replacement = json!({"hash":hash,"raw_transaction":format!("0x{}",hex::encode(&raw)),"prepared_ms":crate::now_ms(),"fees":fees});
        if pending.get("replacements").is_none() {
            pending["replacements"] = json!([]);
        }
        pending["replacements"]
            .as_array_mut()
            .context("invalid replacements")?
            .push(replacement);
        // Persist both original and replacement BEFORE any send. The original nonce
        // reservation remains in place, including if we crash at this exact point.
        self.store.write("pending.json", &pending)?;
        self.store.event("evm_approval_fee_replacement", json!({"original_hash":expected_hash,"hash":hash,"nonce":nonce,"fees":fees,"operation":op}))?;
        tracing::warn!(original_hash=expected_hash, hash=%hash, nonce, fees=?fees, "explicit approval fee replacement prepared; same nonce and calldata");
        self.broadcast(&hash, &raw, nonce).await?;
        self.wait_receipt(expected_hash, &op).await
    }
}
