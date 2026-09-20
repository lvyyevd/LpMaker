use super::{
    UniswapV3,
    abi::{IERC20, INfpm, IRouter02},
    fees::Fees,
    raw_units,
    rpc::{address_word, hex_u64, signed_tick, word_address},
    slippage::{self, Range, parse_sqrt},
};
use crate::{
    domain::{LiquidityVenue, LpPosition},
    math,
    store::Store,
};
use alloy::{
    consensus::{
        SignableTransaction, Transaction, TxEip1559, TxEnvelope, TxLegacy,
        transaction::SignerRecoverable,
    },
    eips::eip2718::{Decodable2718, Encodable2718},
    primitives::{
        Address, Bytes, TxKind, U256,
        aliases::{I24, U24},
        keccak256,
    },
    signers::{SignerSync, local::PrivateKeySigner},
    sol_types::SolCall,
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc, time::Duration};

#[path = "approval_recovery.rs"]
mod approval_recovery;

pub struct Executor {
    pub venue: UniswapV3,
    signer: PrivateKeySigner,
    pub store: Arc<Store>,
    pub nonce: Arc<super::nonce::NonceManager>,
    send_lock: tokio::sync::Mutex<()>,
}

#[async_trait::async_trait]
impl crate::domain::LiquidityExecutor for Executor {
    async fn snapshot(&self) -> Result<crate::domain::PoolSnapshot> {
        self.venue.fresh_snapshot().await
    }
    async fn current_positions(&self) -> Result<Vec<LpPosition>> {
        self.venue
            .positions(&self.owner().to_string(), &self.ids()?)
            .await
    }
    async fn wallet_balances(&self) -> Result<(f64, f64)> {
        self.venue.wallet(self.owner()).await
    }
    fn mint_base_requirement(
        &self,
        value: f64,
        width: f64,
        snapshot: &crate::domain::PoolSnapshot,
    ) -> Result<f64> {
        if self.venue.cfg.chain_id == crate::liquidity::chains::base::CHAIN.id {
            Ok(super::mint::aligned_amounts(&self.venue.cfg, snapshot, value, width)?.0)
        } else {
            Ok(value / 2.0 / snapshot.price)
        }
    }
    async fn mint_layer(&self, layer: &str, value: f64, width: f64) -> Result<()> {
        self.mint(layer, value, width).await?;
        Ok(())
    }
    async fn increase_position(&self, position: &LpPosition, value: f64) -> Result<()> {
        self.increase(position, value).await?;
        Ok(())
    }
    async fn remove_position(&self, position: &LpPosition) -> Result<()> {
        self.remove(position).await?;
        Ok(())
    }
    async fn swap_inventory(&self, sell_base: bool, amount: f64) -> Result<()> {
        self.swap(sell_base, amount).await?;
        Ok(())
    }
    async fn refresh_execution_state(&self) -> Result<()> {
        self.nonce.refresh().await?;
        Ok(())
    }
}
impl Executor {
    /// NFT 的精确 tick 必须来自合约，不能把展示用浮点价格反算为整数 tick。
    async fn position_range(&self, id: U256, block: u64) -> Result<(Range, U256)> {
        let p = self
            .venue
            .position_words(id, &format!("0x{block:x}"))
            .await?;
        let (token0, token1) = if self.venue.base < self.venue.quote {
            (self.venue.base, self.venue.quote)
        } else {
            (self.venue.quote, self.venue.base)
        };
        ensure!(
            word_address(p[2]) == token0
                && word_address(p[3]) == token1
                && p[4] == U256::from(self.venue.cfg.fee),
            "LP NFT pool mismatch"
        );
        Ok((Range::new(signed_tick(p[5]), signed_tick(p[6]))?, p[7]))
    }

    /// 两笔 approve 可能耗时多个区块。额度和 tick 固定，只刷新执行价格及最小量。
    async fn refreshed_entry_snapshot(
        &self,
        before: &crate::domain::PoolSnapshot,
    ) -> Result<crate::domain::PoolSnapshot> {
        let after = self.venue.execution_snapshot().await?;
        slippage::check_price_move(
            parse_sqrt(&before.sqrt_price_x96)?,
            parse_sqrt(&after.sqrt_price_x96)?,
            self.venue.cfg.slippage_bps,
        )?;
        tracing::info!(
            old_block = before.block,
            block = after.block,
            old_price = before.price,
            price = after.price,
            slippage_bps = self.venue.cfg.slippage_bps,
            "LP 授权已完成，已刷新执行报价；投入上限和区间保持原计划"
        );
        Ok(after)
    }

    /// Base 处理池费和报价变化造成的余额尾差；原 Robinhood 路径无额外 RPC。
    async fn funded_liquidity_amounts(&self, rb: U256, rq: U256) -> Result<(U256, U256)> {
        Ok(
            if self.venue.cfg.chain_id == crate::liquidity::chains::base::CHAIN.id {
                let balances = (
                    self.venue.balance(self.venue.base, self.owner()).await?,
                    self.venue.balance(self.venue.quote, self.owner()).await?,
                );
                let amounts = super::mint::fit_balances(rb, rq, balances.0, balances.1)?;
                if amounts != (rb, rq) {
                    tracing::info!(desired_base=%rb, desired_quote=%rq, raw_base=%amounts.0,
                    raw_quote=%amounts.1,"Base 建仓额度已按换币后的真实余额缩小");
                }
                amounts
            } else {
                (rb, rq)
            },
        )
    }
    pub async fn increase(&self, position: &LpPosition, value: f64) -> Result<Value> {
        let snap = self.venue.execution_snapshot().await?;
        let id = U256::from_str_radix(position.token_id.as_deref().context("NFT id")?, 10)?;
        let (range, _) = self.position_range(id, snap.block).await?;
        ensure!(
            snap.price > position.lower && snap.price < position.upper,
            "cannot scale an out-of-range position"
        );
        let l = math::liquidity_for_value(value, position.lower, position.upper, snap.price)?;
        let (b, q) = math::amounts(l, position.lower, position.upper, snap.price);
        let (rb, rq) = (
            raw_units(b, self.venue.cfg.base_decimals)?,
            raw_units(q, self.venue.cfg.quote_decimals)?,
        );
        let (rb, rq) = self.funded_liquidity_amounts(rb, rq).await?;
        self.approve(self.venue.base, self.venue.manager, rb)
            .await?;
        self.approve(self.venue.quote, self.venue.manager, rq)
            .await?;
        let snap = self.refreshed_entry_snapshot(&snap).await?;
        let (a0, a1) = if snap.base_is_token0 {
            (rb, rq)
        } else {
            (rq, rb)
        };
        let (min0, min1) = range.mint_minimums(
            parse_sqrt(&snap.sqrt_price_x96)?,
            a0,
            a1,
            self.venue.cfg.slippage_bps,
        )?;
        let params = INfpm::IncreaseLiquidityParams {
            tokenId: id,
            amount0Desired: a0,
            amount1Desired: a1,
            amount0Min: min0,
            amount1Min: min1,
            deadline: U256::from(crate::now_ms() / 1000 + self.venue.cfg.deadline_seconds),
        };
        self.send(
            self.venue.manager,
            INfpm::increaseLiquidityCall { params }.abi_encode(),
            json!({"kind":"increase","layer":position.layer,"token_id":position.token_id,"value_usdg":value,"lower":position.lower,"upper":position.upper,"price":snap.price,"quote_time_ms":snap.time_ms,"quote_block":snap.block,"amount0_min":min0.to_string(),"amount1_min":min1.to_string(),"slippage_bps":self.venue.cfg.slippage_bps}),
        )
        .await
    }
    pub fn new(venue: UniswapV3, store: Arc<Store>) -> Result<Self> {
        let key = std::env::var(&venue.cfg.private_key_env)
            .context("missing EVM signing environment variable")?;
        let signer: PrivateKeySigner = key.parse().context("invalid EVM signer")?;
        Self::with_signer(venue, store, signer)
    }
    /// Injectable signer for offline transaction-lifecycle tests; CLI still enforces live + --execute.
    pub fn with_signer(
        venue: UniswapV3,
        store: Arc<Store>,
        signer: PrivateKeySigner,
    ) -> Result<Self> {
        if let Some(identity) = store.read::<Value>("execution_identity.json")? {
            ensure!(
                identity["chain_id"] == venue.cfg.chain_id
                    && identity["owner"]
                        .as_str()
                        .is_some_and(|o| o.eq_ignore_ascii_case(&signer.address().to_string())),
                "execution state belongs to another LP wallet/chain"
            );
        }
        if let Some(owner) = &venue.cfg.owner {
            ensure!(
                owner.parse::<Address>()? == signer.address(),
                "configured LP owner differs from signing wallet"
            );
        }
        let nonce = Arc::new(super::nonce::NonceManager::new(
            venue.rpc.clone(),
            signer.address(),
            venue.cfg.chain_id,
            venue.cfg.pending_warn_seconds,
            store.clone(),
        ));
        store.write(
            "execution_identity.json",
            &json!({"owner":signer.address(),"chain_id":venue.cfg.chain_id}),
        )?;
        Ok(Self {
            venue,
            signer,
            store,
            nonce,
            send_lock: tokio::sync::Mutex::new(()),
        })
    }
    pub fn owner(&self) -> Address {
        self.signer.address()
    }
    pub fn ids(&self) -> Result<Vec<(String, String)>> {
        Ok(self
            .store
            .read::<BTreeMap<String, String>>("nfts.json")?
            .unwrap_or_default()
            .into_iter()
            .collect())
    }
    pub async fn verify_inventory(&self) -> Result<()> {
        let expected = self
            .ids()?
            .into_iter()
            .map(|(_, id)| id)
            .collect::<std::collections::BTreeSet<_>>();
        let actual = self
            .venue
            .token_ids(self.owner())
            .await?
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>();
        self.store.write("lp_inventory.json", &json!({"observed_ms":crate::now_ms(),
            "owner":self.owner(),"expected":expected,"actual_active_nfts":actual,"consistent":actual == expected}))?;
        ensure!(
            actual == expected,
            "untracked/missing LP NFTs: import existing positions before strategy startup (expected {expected:?}, on-chain {actual:?})"
        );
        Ok(())
    }
    pub async fn approve(&self, token: Address, spender: Address, amount: U256) -> Result<()> {
        let allowance = self
            .venue
            .rpc
            .words(
                token,
                "allowance(address,address)",
                &[address_word(self.owner()), address_word(spender)],
                "latest",
            )
            .await?[0];
        if allowance >= amount {
            return Ok(());
        }
        if allowance > U256::ZERO {
            self.send(
                token,
                IERC20::approveCall {
                    spender,
                    value: U256::ZERO,
                }
                .abi_encode(),
                json!({"kind":"approve_zero","token":token,"spender":spender,"raw_amount":"0"}),
            )
            .await?;
        }
        self.send(
            token,
            IERC20::approveCall {
                spender,
                value: amount,
            }
            .abi_encode(),
            json!({"kind":"approve","token":token,"spender":spender,"raw_amount":amount.to_string()}),
        )
        .await?;
        Ok(())
    }
    pub async fn send(&self, to: Address, data: Vec<u8>, operation: Value) -> Result<Value> {
        let _guard = self.send_lock.lock().await;
        tracing::info!(target=%to, operation=%operation, "EVM operation started");
        let result = self.send_inner(to, data, operation).await;
        if let Err(e) = &result {
            tracing::error!(error=%format!("{e:#}"), "EVM operation failed; inspect pending state before retry");
        }
        result
    }
    async fn send_inner(&self, to: Address, data: Vec<u8>, operation: Value) -> Result<Value> {
        if let Some(quote_time) = operation.get("quote_time_ms") {
            crate::runtime::fresh(
                quote_time.as_u64().context("invalid quote time")?,
                crate::now_ms(),
                self.venue.cfg.max_quote_age_seconds,
            )?;
        }
        ensure!(
            [
                self.venue.manager,
                self.venue.router,
                self.venue.base,
                self.venue.quote
            ]
            .contains(&to),
            "transaction target not allowlisted"
        );
        let rpc = &self.venue.rpc;
        ensure!(
            hex_u64(&rpc.request("eth_chainId", json!([])).await?)? == self.venue.cfg.chain_id,
            "chain changed"
        );
        let nonce = self.nonce.next().await?;
        let call = json!({"from":self.owner(),"to":to,"data":format!("0x{}",hex::encode(&data)),"value":"0x0"});
        if let Err(error) = rpc.request("eth_call", json!([call, "latest"])).await {
            tracing::error!(nonce, operation=%operation, error=%error,
                "EVM 预检失败：本次交易尚未签名或广播；此前步骤可能已完成，请保留状态");
            return Err(error.context("transaction simulation failed before signing/broadcast; earlier workflow operations may have completed"));
        }
        let estimated_gas = hex_u64(&rpc.request("eth_estimateGas", json!([call])).await?)?;
        let gas = u64::try_from(super::fees::buffered(
            u128::from(estimated_gas),
            self.venue.cfg.gas_limit_buffer_bps,
        )?)
        .context("buffered gas limit overflow")?;
        let fees = Fees::estimate(rpc, self.venue.cfg.gas_fee_buffer_bps).await?;
        tracing::info!(nonce, estimated_gas, gas, gas_limit_buffer_bps=self.venue.cfg.gas_limit_buffer_bps, fees=?fees, operation=%operation, "EVM simulation and buffered gas estimation completed");
        self.check_gas_budget(gas, &fees, data.len()).await?;
        // Approval, simulation and gas RPCs can consume most of the quote lifetime.
        if let Some(quote_time) = operation.get("quote_time_ms") {
            crate::runtime::fresh(
                quote_time.as_u64().context("invalid quote time")?,
                crate::now_ms(),
                self.venue.cfg.max_quote_age_seconds,
            )?;
        }
        let raw = self.sign_transaction(nonce, gas, to, data, &fees)?;
        let hash = format!("{:#x}", keccak256(&raw));
        self.store.begin(json!({"venue":"evm","hash":hash,"nonce":nonce,"owner":self.owner(),"prepared_ms":crate::now_ms(),"operation":operation,"fees":fees,"raw_transaction":format!("0x{}",hex::encode(&raw))}))?;
        self.nonce.prepared(nonce, &hash).await?;
        self.broadcast(&hash, &raw, nonce).await?;
        self.wait_receipt(&hash, &operation).await
    }
    async fn check_gas_budget(&self, gas: u64, fees: &Fees, calldata_len: usize) -> Result<()> {
        let extra = if self.venue.cfg.chain_id == crate::liquidity::chains::base::CHAIN.id {
            crate::liquidity::chains::base::fees::extra_fee(
                &self.venue.rpc,
                calldata_len,
                gas,
                self.venue.cfg.gas_fee_buffer_bps,
            )
            .await?
        } else {
            U256::ZERO
        };
        let required = (U256::from(gas) * U256::from(fees.max_fee_per_gas))
            .checked_add(extra)
            .context("total gas fee overflow")?;
        ensure!(
            (gas as f64) * (fees.max_fee_per_gas as f64) / 1e18 + super::units(extra, 18)?
                <= self.venue.cfg.max_gas_native,
            "transaction gas budget exceeded"
        );
        let balance = self
            .venue
            .rpc
            .request("eth_getBalance", json!([self.owner(), "latest"]))
            .await?;
        let balance = U256::from_str_radix(
            balance
                .as_str()
                .context("ETH balance")?
                .trim_start_matches("0x"),
            16,
        )?;
        ensure!(balance >= required, "insufficient native ETH for gas");
        Ok(())
    }
    fn sign_transaction(
        &self,
        nonce: u64,
        gas: u64,
        to: Address,
        data: Vec<u8>,
        fees: &Fees,
    ) -> Result<Vec<u8>> {
        if let Some(tip) = fees.max_priority_fee_per_gas {
            let tx = TxEip1559 {
                chain_id: self.venue.cfg.chain_id,
                nonce,
                gas_limit: gas,
                max_fee_per_gas: fees.max_fee_per_gas,
                max_priority_fee_per_gas: tip,
                to: TxKind::Call(to),
                value: U256::ZERO,
                input: Bytes::from(data),
                access_list: Default::default(),
            };
            let sig = self.signer.sign_hash_sync(&tx.signature_hash())?;
            return Ok(tx.into_signed(sig).encoded_2718());
        }
        let tx = TxLegacy {
            chain_id: Some(self.venue.cfg.chain_id),
            nonce,
            gas_price: fees.max_fee_per_gas,
            gas_limit: gas,
            to: TxKind::Call(to),
            value: U256::ZERO,
            input: Bytes::from(data),
        };
        let sig = self.signer.sign_hash_sync(&tx.signature_hash())?;
        Ok(tx.into_signed(sig).encoded_2718())
    }
    async fn broadcast(&self, hash: &str, raw: &[u8], nonce: u64) -> Result<()> {
        self.venue.observations.invalidate();
        let result = self
            .venue
            .rpc
            .request(
                "eth_sendRawTransaction",
                json!([format!("0x{}", hex::encode(raw))]),
            )
            .await;
        let sent = match result {
            Ok(value) => value,
            Err(error) => {
                let mut pending = self.store.pending()?.context("missing broadcast intent")?;
                let details = json!({"hash":hash,"nonce":nonce,"time_ms":crate::now_ms(),"rpc_rejection":error.is::<super::rpc::RpcError>(),"error":format!("{error:#}")});
                pending["last_broadcast_error"] = details.clone();
                self.store.write("pending.json", &pending)?;
                self.store.event("evm_broadcast_error", &details)?;
                return Err(error).context("broadcast not confirmed; pending retained, reconcile before retrying; never resend with a new nonce");
            }
        };
        ensure!(
            sent.as_str().is_some_and(|s| s.eq_ignore_ascii_case(hash)),
            "RPC returned unexpected transaction hash"
        );
        self.store
            .event("evm_broadcast", json!({"hash":hash,"nonce":nonce}))?;
        tracing::info!(%hash, nonce, "EVM transaction broadcast acknowledged");
        Ok(())
    }
    pub async fn wait_receipt(&self, hash: &str, operation: &Value) -> Result<Value> {
        let hashes = if let Some(pending) = self.store.pending()? {
            let hashes = self.validated_pending_hashes(&pending)?;
            ensure!(
                hashes.iter().any(|h| h.eq_ignore_ascii_case(hash)),
                "pending hash mismatch"
            );
            hashes
        } else {
            vec![hash.to_string()]
        };
        let mut refreshed = std::time::Instant::now();
        for _ in 0..90 {
            if refreshed.elapsed() >= Duration::from_secs(self.venue.cfg.nonce_refresh_seconds) {
                self.nonce.refresh().await?;
                refreshed = std::time::Instant::now();
            }
            for hash in &hashes {
                let r = self
                    .venue
                    .rpc
                    .request("eth_getTransactionReceipt", json!([hash]))
                    .await?;
                if !r.is_null() {
                    ensure!(
                        r["transactionHash"]
                            .as_str()
                            .is_some_and(|h| h.eq_ignore_ascii_case(hash)),
                        "receipt hash mismatch; nonce remains unresolved"
                    );
                    let block = hex_u64(&r["blockNumber"])?;
                    tracing::debug!(
                        hash=%hash,
                        block,
                        "EVM receipt observed; waiting for confirmations"
                    );
                    if self.venue.rpc.block_number().await? >= block + self.venue.cfg.confirmations
                    {
                        let canonical = self
                            .venue
                            .rpc
                            .request("eth_getBlockByNumber", json!([r["blockNumber"], false]))
                            .await?;
                        ensure!(
                            canonical["hash"] == r["blockHash"],
                            "receipt reorged; reconcile required"
                        );
                        if hex_u64(&r["status"])? != 1 {
                            self.venue.observations.invalidate();
                            self.nonce.confirmed(hash, &r).await?;
                            bail!("on-chain transaction reverted: {hash}")
                        }
                        self.record_receipt(&r, operation)?;
                        self.venue.observations.invalidate();
                        self.nonce.confirmed(hash, &r).await?;
                        tracing::info!(hash=%hash, block, operation=%operation, "EVM operation confirmed");
                        return Ok(r);
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        bail!("transaction still unresolved: {hash}; run reconcile")
    }
    fn record_receipt(&self, r: &Value, op: &Value) -> Result<()> {
        let mut ids = self
            .store
            .read::<BTreeMap<String, String>>("nfts.json")?
            .unwrap_or_default();
        if op["kind"] == "mint" {
            let topic = format!("{:#x}", keccak256("Transfer(address,address,uint256)"));
            let zero = format!("0x{}", "0".repeat(64));
            let owner_topic = format!("0x{:0>64}", hex::encode(self.owner()));
            let log = r["logs"]
                .as_array()
                .context("receipt logs")?
                .iter()
                .find(|l| {
                    l["address"]
                        .as_str()
                        .is_some_and(|a| a.eq_ignore_ascii_case(&self.venue.cfg.position_manager))
                        && l["topics"][0] == topic
                        && l["topics"][1] == zero
                        && l["topics"][2] == owner_topic
                })
                .context("mint receipt missing NFT Transfer")?;
            let id = U256::from_str_radix(
                log["topics"][3]
                    .as_str()
                    .context("NFT id")?
                    .trim_start_matches("0x"),
                16,
            )?;
            ids.insert(
                op["layer"].as_str().context("mint layer")?.into(),
                id.to_string(),
            );
            crate::recovery::record_lp_history(
                &self.store,
                json!({"source":"confirmed_mint","token_id":id.to_string(),"hash":r["transactionHash"]}),
            )?;
        } else if op["kind"] == "burn" {
            ids.remove(op["layer"].as_str().context("burn layer")?);
        }
        self.store.write("nfts.json", &ids)
    }
    pub async fn mint(&self, layer: &str, value: f64, width: f64) -> Result<Value> {
        ensure!(
            !self.ids()?.iter().any(|(l, _)| l == layer),
            "layer already has an NFT"
        );
        let snap = self.venue.execution_snapshot().await?;
        let (lo, hi) = math::range(snap.price, width);
        let (tl, tu) = math::aligned_ticks(
            lo,
            hi,
            snap.tick_spacing,
            self.venue.cfg.base_decimals,
            self.venue.cfg.quote_decimals,
            snap.base_is_token0,
        )?;
        let a = math::tick_to_price(
            tl,
            self.venue.cfg.base_decimals,
            self.venue.cfg.quote_decimals,
            snap.base_is_token0,
        );
        let b = math::tick_to_price(
            tu,
            self.venue.cfg.base_decimals,
            self.venue.cfg.quote_decimals,
            snap.base_is_token0,
        );
        let l = math::liquidity_for_value(value, a.min(b), a.max(b), snap.price)?;
        let (base, quote) = math::amounts(l, a.min(b), a.max(b), snap.price);
        let rb = raw_units(base, self.venue.cfg.base_decimals)?;
        let rq = raw_units(quote, self.venue.cfg.quote_decimals)?;
        let (rb, rq) = self.funded_liquidity_amounts(rb, rq).await?;
        ensure!(
            self.venue.balance(self.venue.base, self.owner()).await? >= rb
                && self.venue.balance(self.venue.quote, self.owner()).await? >= rq,
            "insufficient token balances for mint"
        );
        self.approve(self.venue.base, self.venue.manager, rb)
            .await?;
        self.approve(self.venue.quote, self.venue.manager, rq)
            .await?;
        let snap = self.refreshed_entry_snapshot(&snap).await?;
        let (t0, t1, a0, a1) = if snap.base_is_token0 {
            (self.venue.base, self.venue.quote, rb, rq)
        } else {
            (self.venue.quote, self.venue.base, rq, rb)
        };
        let (min0, min1) = Range::new(tl, tu)?.mint_minimums(
            parse_sqrt(&snap.sqrt_price_x96)?,
            a0,
            a1,
            self.venue.cfg.slippage_bps,
        )?;
        let params = INfpm::MintParams {
            token0: t0,
            token1: t1,
            fee: U24::from(self.venue.cfg.fee),
            tickLower: I24::try_from(tl)?,
            tickUpper: I24::try_from(tu)?,
            amount0Desired: a0,
            amount1Desired: a1,
            amount0Min: min0,
            amount1Min: min1,
            recipient: self.owner(),
            deadline: U256::from(crate::now_ms() / 1000 + self.venue.cfg.deadline_seconds),
        };
        self.send(
            self.venue.manager,
            INfpm::mintCall { params }.abi_encode(),
            json!({"kind":"mint","layer":layer,"value_usdg":value,"half_width":width,"tick_lower":tl,"tick_upper":tu,"raw_base":rb.to_string(),"raw_quote":rq.to_string(),"price":snap.price,"quote_time_ms":snap.time_ms,"quote_block":snap.block,"amount0_min":min0.to_string(),"amount1_min":min1.to_string(),"slippage_bps":self.venue.cfg.slippage_bps}),
        )
        .await
    }
    pub async fn collect(&self, id: &str) -> Result<Value> {
        let params = INfpm::CollectParams {
            tokenId: U256::from_str_radix(id, 10)?,
            recipient: self.owner(),
            amount0Max: u128::MAX,
            amount1Max: u128::MAX,
        };
        self.send(
            self.venue.manager,
            INfpm::collectCall { params }.abi_encode(),
            json!({"kind":"collect","token_id":id}),
        )
        .await
    }
    pub async fn remove(&self, pos: &LpPosition) -> Result<Value> {
        let id = U256::from_str_radix(
            pos.token_id
                .as_deref()
                .context("live position needs token id")?,
            10,
        )?;
        let snap = self.venue.execution_snapshot().await?;
        let (range, current_liquidity) = self.position_range(id, snap.block).await?;
        let raw_liquidity = U256::from_str_radix(&pos.raw_liquidity, 10)?;
        ensure!(
            raw_liquidity == current_liquidity,
            "LP liquidity changed since observation; reconcile before removal"
        );
        let (min0, min1) = range.burn_minimums(
            parse_sqrt(&snap.sqrt_price_x96)?,
            raw_liquidity,
            self.venue.cfg.slippage_bps,
        )?;
        let raw = raw_liquidity.to::<u128>();
        let mut calls = vec![];
        if raw > 0 {
            calls.push(Bytes::from(
                INfpm::decreaseLiquidityCall {
                    params: INfpm::DecreaseLiquidityParams {
                        tokenId: id,
                        liquidity: raw,
                        amount0Min: min0,
                        amount1Min: min1,
                        deadline: U256::from(
                            crate::now_ms() / 1000 + self.venue.cfg.deadline_seconds,
                        ),
                    },
                }
                .abi_encode(),
            ));
        }
        calls.push(Bytes::from(
            INfpm::collectCall {
                params: INfpm::CollectParams {
                    tokenId: id,
                    recipient: self.owner(),
                    amount0Max: u128::MAX,
                    amount1Max: u128::MAX,
                },
            }
            .abi_encode(),
        ));
        calls.push(Bytes::from(INfpm::burnCall { tokenId: id }.abi_encode()));
        self.send(
            self.venue.manager,
            INfpm::multicallCall { data: calls }.abi_encode(),
            json!({"kind":"burn","layer":pos.layer,"token_id":pos.token_id,"raw_liquidity":pos.raw_liquidity,"lower":pos.lower,"upper":pos.upper,"price":snap.price,"quote_time_ms":snap.time_ms,"quote_block":snap.block,"amount0_min":min0.to_string(),"amount1_min":min1.to_string(),"slippage_bps":self.venue.cfg.slippage_bps}),
        )
        .await
    }
    pub async fn swap(&self, sell_base: bool, amount: f64) -> Result<Value> {
        self.swap_amount(sell_base, Some(amount)).await
    }
    /// 显式退出使用精确余额，避免 f64 换算留下几个 wei，或盲目增加数量造成超支。
    pub async fn sell_all_base(&self) -> Result<Value> {
        self.swap_amount(true, None).await
    }
    async fn swap_amount(&self, sell_base: bool, amount: Option<f64>) -> Result<Value> {
        // 兑换限价仍需独立核对确认块，不能复用监控的短期快照。
        let s = self.venue.fresh_snapshot().await?;
        let (token_in, token_out, di, do_, rate) = if sell_base {
            (
                self.venue.base,
                self.venue.quote,
                self.venue.cfg.base_decimals,
                self.venue.cfg.quote_decimals,
                s.price,
            )
        } else {
            (
                self.venue.quote,
                self.venue.base,
                self.venue.cfg.quote_decimals,
                self.venue.cfg.base_decimals,
                1.0 / s.price,
            )
        };
        let balance = self.venue.balance(token_in, self.owner()).await?;
        let input = match amount {
            Some(amount) => super::bounded_amount(amount, di, balance)?,
            None => balance,
        };
        ensure!(input > U256::ZERO, "zero swap");
        let expected = super::units(input, di)? * rate;
        let min_out = self
            .venue
            .minimum_swap_output(token_in, token_out, input, s.block, expected, do_)
            .await?;
        ensure!(min_out > U256::ZERO, "zero minimum output");
        self.approve(token_in, self.venue.router, input).await?;
        let inner = IRouter02::exactInputSingleCall {
            params: IRouter02::ExactInputSingleParams {
                tokenIn: token_in,
                tokenOut: token_out,
                fee: U24::from(self.venue.cfg.fee),
                recipient: self.owner(),
                amountIn: input,
                amountOutMinimum: min_out,
                sqrtPriceLimitX96: Default::default(),
            },
        }
        .abi_encode();
        let data = IRouter02::multicallCall {
            deadline: U256::from(crate::now_ms() / 1000 + self.venue.cfg.deadline_seconds),
            data: vec![Bytes::from(inner)],
        }
        .abi_encode();
        self.send(
            self.venue.router,
            data,
            json!({"kind":"swap","sell_base":sell_base,"input_token":token_in,"output_token":token_out,"raw_input":input.to_string(),"raw_min_output":min_out.to_string(),"price":s.price,"quote_time_ms":s.time_ms,"slippage_bps":self.venue.cfg.slippage_bps}),
        )
        .await
    }
}
