//! Uniswap V3 共用适配器：负责 NFT、tick、代币精度和合约调用，策略不依赖这些细节。
pub mod abi;
pub mod events;
pub(crate) use crate::evm::{fees, nonce, rpc};
pub mod mint;
pub mod observations;
pub mod quote;
pub mod slippage;
pub mod tx;
use crate::{
    config::LiquidityConfig,
    domain::{LiquidityVenue, LpPosition, PoolSnapshot},
    math,
};
use alloy::primitives::{Address, U256, U512};
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use rpc::{Rpc, address_word, signed_tick, tick_word, word_address};
use serde_json::{Value, json};
use std::sync::Arc;

#[derive(Clone)]
pub struct UniswapV3 {
    pub cfg: LiquidityConfig,
    pub rpc: Rpc,
    pub archive_rpc: Rpc,
    pub pool: Address,
    pub manager: Address,
    pub router: Address,
    pub base: Address,
    pub quote: Address,
    pub observations: Arc<observations::Shared>,
}
impl UniswapV3 {
    async fn snapshot_at(&self, number: u64) -> Result<PoolSnapshot> {
        let tag = format!("0x{number:x}");
        let header = self
            .rpc
            .request("eth_getBlockByNumber", json!([tag, false]))
            .await?;
        self.snapshot_with_header(number, &header).await
    }
    async fn tick_spacing(&self) -> Result<i32> {
        let _guard = self.observations.spacing_lock.lock().await;
        if let Some(spacing) = self.observations.spacing() {
            return Ok(spacing);
        }
        let spacing = signed_tick(
            self.rpc
                .words(self.pool, "tickSpacing()", &[], "latest")
                .await?[0],
        );
        ensure!(spacing > 0, "invalid pool tick spacing");
        self.observations.set_spacing(spacing);
        Ok(spacing)
    }
    async fn snapshot_with_header(&self, number: u64, header: &Value) -> Result<PoolSnapshot> {
        let tag = format!("0x{number:x}");
        let slot = self.rpc.words(self.pool, "slot0()", &[], &tag).await?;
        ensure!(
            slot.len() == 7 && slot[6] != U256::ZERO,
            "uninitialized/locked pool"
        );
        let l = self.rpc.words(self.pool, "liquidity()", &[], &tag).await?[0];
        let spacing = self.tick_spacing().await?;
        let base0 = self.base < self.quote;
        let sqrt = slot[0].to_string().parse::<f64>()? / 2_f64.powi(96);
        let raw = sqrt * sqrt;
        let price = (if base0 { raw } else { 1.0 / raw })
            * 10_f64.powi(self.cfg.base_decimals as i32 - self.cfg.quote_decimals as i32);
        ensure!(
            price.is_finite() && price > 0.0 && spacing > 0 && l > U256::ZERO,
            "invalid pool state"
        );
        Ok(PoolSnapshot {
            block: number,
            block_hash: header["hash"].as_str().context("block hash")?.into(),
            time_ms: rpc::hex_u64(&header["timestamp"])? * 1000,
            price,
            tick: signed_tick(slot[1]),
            tick_spacing: spacing,
            liquidity: l.to_string(),
            sqrt_price_x96: slot[0].to_string(),
            base_is_token0: base0,
        })
    }

    /// 监控/记账仍使用确认块；发送交易前报价单独读取最新块，避免确认延迟造成旧报价。
    pub async fn execution_snapshot(&self) -> Result<PoolSnapshot> {
        self.snapshot_at(self.rpc.block_number().await?).await
    }

    /// 明确的执行/恢复读取不使用只读快照缓存；保留原确认数。
    pub async fn fresh_snapshot(&self) -> Result<PoolSnapshot> {
        self.snapshot_at(
            self.rpc
                .block_number()
                .await?
                .saturating_sub(self.cfg.confirmations),
        )
        .await
    }

    /// 同一确认块的重复合约读取合并；latest 余额、nonce、报价及签名预检不走缓存。
    async fn words_at(
        &self,
        snap: &PoolSnapshot,
        to: Address,
        signature: &str,
        args: &[U256],
    ) -> Result<Vec<U256>> {
        let key = json!([snap.block, snap.block_hash, to, signature, args]).to_string();
        let _guard = self.observations.read_lock.lock().await;
        if let Some(words) = self.observations.words(&key) {
            return Ok(words);
        }
        let epoch = self.observations.epoch();
        let words = self
            .rpc
            .words(to, signature, args, &format!("0x{:x}", snap.block))
            .await?;
        self.observations.ensure_epoch(epoch)?;
        self.observations.save_words(epoch, key, words.clone());
        Ok(words)
    }

    pub async fn canonical_hash(&self, number: u64) -> Result<String> {
        if let Some(hash) = self.observations.canonical_hash(number) {
            return Ok(hash);
        }
        Ok(self
            .rpc
            .request(
                "eth_getBlockByNumber",
                json!([format!("0x{number:x}"), false]),
            )
            .await?["hash"]
            .as_str()
            .context("block hash")?
            .to_string())
    }

    pub fn new(cfg: LiquidityConfig) -> Result<Self> {
        Ok(Self {
            observations: observations::Shared::for_pool(&cfg),
            rpc: Rpc::with_min_interval(cfg.rpc_url.clone(), cfg.rpc_min_interval_ms)?,
            archive_rpc: Rpc::with_min_interval(
                cfg.archive_rpc_url
                    .clone()
                    .unwrap_or_else(|| cfg.rpc_url.clone()),
                cfg.rpc_min_interval_ms,
            )?,
            pool: cfg.pool.parse()?,
            manager: cfg.position_manager.parse()?,
            router: cfg.swap_router.parse()?,
            base: cfg.base_token.parse()?,
            quote: cfg.quote_token.parse()?,
            cfg,
        })
    }
    pub async fn balance(&self, token: Address, owner: Address) -> Result<U256> {
        Ok(self
            .rpc
            .words(
                token,
                "balanceOf(address)",
                &[address_word(owner)],
                "latest",
            )
            .await?[0])
    }
    pub async fn wallet(&self, owner: Address) -> Result<(f64, f64)> {
        Ok((
            units(
                self.balance(self.base, owner).await?,
                self.cfg.base_decimals,
            )?,
            units(
                self.balance(self.quote, owner).await?,
                self.cfg.quote_decimals,
            )?,
        ))
    }
    pub async fn token_ids(&self, owner: Address) -> Result<Vec<String>> {
        self.token_ids_at(owner, "latest").await
    }
    pub async fn token_ids_at(&self, owner: Address, block: &str) -> Result<Vec<String>> {
        self.matching_token_ids_at(owner, block, false).await
    }
    /// 手工退出包含已经移除流动性、但仍可能留有待领手续费的 NFT。
    pub async fn exit_token_ids(&self, owner: Address) -> Result<Vec<String>> {
        let block = format!("0x{:x}", self.rpc.block_number().await?);
        self.matching_token_ids_at(owner, &block, true).await
    }
    async fn matching_token_ids_at(
        &self,
        owner: Address,
        block: &str,
        include_empty: bool,
    ) -> Result<Vec<String>> {
        self.observations.watch_owner(owner);
        let epoch = self.observations.epoch();
        let count = self
            .rpc
            .words(
                self.manager,
                "balanceOf(address)",
                &[address_word(owner)],
                block,
            )
            .await?[0]
            .to::<u64>();
        ensure!(
            count <= 1000,
            "use a dedicated wallet with at most 1000 positions"
        );
        let mut ids = vec![];
        for i in 0..count {
            let id = self
                .rpc
                .words(
                    self.manager,
                    "tokenOfOwnerByIndex(address,uint256)",
                    &[address_word(owner), U256::from(i)],
                    block,
                )
                .await?[0];
            let p = self.position_words(id, block).await?;
            let t0 = word_address(p[2]);
            let t1 = word_address(p[3]);
            if ((t0 == self.base && t1 == self.quote) || (t0 == self.quote && t1 == self.base))
                && p[4] == U256::from(self.cfg.fee)
                && (include_empty || p[7] > U256::ZERO)
            {
                ids.push(id.to_string());
            }
        }
        self.observations.ensure_epoch(epoch)?;
        self.observations.inventory_checked(epoch, owner, &ids);
        Ok(ids)
    }
    pub async fn position_words(&self, id: U256, block: &str) -> Result<Vec<U256>> {
        let p = self
            .rpc
            .words(self.manager, "positions(uint256)", &[id], block)
            .await?;
        ensure!(p.len() == 12, "invalid NFPM position");
        Ok(p)
    }
    pub async fn logs(&self, from: u64, to: u64) -> Result<Value> {
        ensure!(
            to >= from && to - from <= 1999,
            "log request limited to 2000 blocks"
        );
        self.archive_rpc.request("eth_getLogs",json!([{"address":self.pool,"fromBlock":format!("0x{from:x}"),"toBlock":format!("0x{to:x}")}])).await
    }
    async fn unclaimed(&self, position: &[U256], snap: &PoolSnapshot) -> Result<(U256, U256)> {
        let tick = snap.tick;
        let lower = self
            .words_at(
                snap,
                self.pool,
                "ticks(int24)",
                &[tick_word(signed_tick(position[5]))],
            )
            .await?;
        let upper = self
            .words_at(
                snap,
                self.pool,
                "ticks(int24)",
                &[tick_word(signed_tick(position[6]))],
            )
            .await?;
        ensure!(lower.len() >= 4 && upper.len() >= 4, "tick ABI mismatch");
        let mut out = [U256::ZERO; 2];
        for i in 0..2 {
            let global = self
                .words_at(
                    snap,
                    self.pool,
                    if i == 0 {
                        "feeGrowthGlobal0X128()"
                    } else {
                        "feeGrowthGlobal1X128()"
                    },
                    &[],
                )
                .await?[0];
            let below = if tick >= signed_tick(position[5]) {
                lower[2 + i]
            } else {
                global.wrapping_sub(lower[2 + i])
            };
            let above = if tick < signed_tick(position[6]) {
                upper[2 + i]
            } else {
                global.wrapping_sub(upper[2 + i])
            };
            let inside = global.wrapping_sub(below).wrapping_sub(above);
            let delta = inside.wrapping_sub(position[8 + i]);
            let earned = (U512::from(delta) * U512::from(position[7])) >> 128;
            ensure!(earned <= U512::from(U256::MAX), "fee amount overflow");
            out[i] = U256::from(earned)
                .checked_add(position[10 + i])
                .context("fee overflow")?;
        }
        Ok((out[0], out[1]))
    }
}
#[async_trait]
impl LiquidityVenue for UniswapV3 {
    async fn validate(&self) -> Result<()> {
        let chain = rpc::hex_u64(&self.rpc.request("eth_chainId", json!([])).await?)?;
        ensure!(chain == self.cfg.chain_id, "wrong RPC chain ID");
        let expected: Address = self.cfg.factory.parse()?;
        for a in [
            self.pool,
            self.manager,
            self.router,
            self.base,
            self.quote,
            expected,
        ] {
            let code = self
                .rpc
                .request("eth_getCode", json!([a, "latest"]))
                .await?;
            ensure!(
                code.as_str().is_some_and(|s| s.len() > 2),
                "contract missing at {a}"
            );
        }
        let t0 = word_address(self.rpc.words(self.pool, "token0()", &[], "latest").await?[0]);
        let t1 = word_address(self.rpc.words(self.pool, "token1()", &[], "latest").await?[0]);
        ensure!(
            (t0 == self.base && t1 == self.quote) || (t0 == self.quote && t1 == self.base),
            "unexpected pool tokens"
        );
        ensure!(
            self.rpc.words(self.pool, "fee()", &[], "latest").await?[0] == U256::from(self.cfg.fee),
            "wrong fee tier"
        );
        for a in [self.pool, self.manager] {
            ensure!(
                word_address(self.rpc.words(a, "factory()", &[], "latest").await?[0]) == expected,
                "factory mismatch"
            );
        }
        ensure!(
            word_address(
                self.rpc
                    .words(self.router, "factory()", &[], "latest")
                    .await?[0]
            ) == expected,
            "router factory mismatch"
        );
        ensure!(
            word_address(
                self.rpc
                    .words(
                        expected,
                        "getPool(address,address,uint24)",
                        &[address_word(t0), address_word(t1), U256::from(self.cfg.fee)],
                        "latest"
                    )
                    .await?[0]
            ) == self.pool,
            "factory pool registration mismatch"
        );
        ensure!(
            self.rpc
                .words(self.base, "decimals()", &[], "latest")
                .await?[0]
                == U256::from(self.cfg.base_decimals),
            "base decimals mismatch"
        );
        ensure!(
            self.rpc
                .words(self.quote, "decimals()", &[], "latest")
                .await?[0]
                == U256::from(self.cfg.quote_decimals),
            "quote decimals mismatch"
        );
        // 新接入链单独核对 Quoter 和 tick 间距，不改变 Robinhood 的旧请求序列。
        if let Some(quoter) = self.quoter()? {
            ensure!(
                word_address(self.rpc.words(quoter, "factory()", &[], "latest").await?[0])
                    == expected,
                "quoter factory mismatch"
            );
            if let Some(pool) = crate::liquidity::chains::pool(&self.cfg) {
                ensure!(
                    self.rpc
                        .words(self.pool, "tickSpacing()", &[], "latest")
                        .await?[0]
                        == U256::from(pool.tick_spacing as u32),
                    "registered pool tick spacing mismatch"
                );
            }
        }
        Ok(())
    }
    async fn snapshot(&self) -> Result<PoolSnapshot> {
        let _guard = self.observations.snapshot_lock.lock().await;
        if let Some(snapshot) = self.observations.snapshot() {
            return Ok(snapshot);
        }
        let epoch = self.observations.epoch();
        let snapshot = if let Some((head, anchor_needed)) =
            self.observations.confirmed_header(self.cfg.confirmations)
        {
            if anchor_needed {
                let canonical = self
                    .rpc
                    .request(
                        "eth_getBlockByNumber",
                        json!([format!("0x{:x}", head.number), false]),
                    )
                    .await?;
                if !canonical["hash"]
                    .as_str()
                    .is_some_and(|hash| hash.eq_ignore_ascii_case(&head.hash))
                    || rpc::hex_u64(&canonical["timestamp"])
                        .ok()
                        .and_then(|t| t.checked_mul(1000))
                        != Some(head.time_ms)
                {
                    self.observations.reject_feed();
                    return Err(crate::runtime::ReadUnavailable("WSS/RPC block mismatch or RPC behind; discard stream anchor and reconcile before acting".into()).into());
                }
                self.observations.anchored(epoch);
            }
            self.snapshot_with_header(
                head.number,
                &json!({"hash":head.hash,"timestamp":format!("0x{:x}",head.time_ms/1000)}),
            )
            .await?
        } else {
            self.fresh_snapshot().await?
        };
        self.observations.ensure_epoch(epoch)?;
        self.observations.save_snapshot(epoch, &snapshot);
        Ok(snapshot)
    }
    async fn positions(&self, owner: &str, ids: &[(String, String)]) -> Result<Vec<LpPosition>> {
        let snap = self.snapshot().await?;
        Ok(self
            .position_observations(owner, ids, &snap)
            .await?
            .into_iter()
            .map(|(p, _)| p)
            .collect())
    }
}
impl UniswapV3 {
    /// Position amounts and accounting revision all come from the same confirmed block.
    /// A revision change breaks fee-rate sampling across collection/liquidity operations.
    pub async fn position_observations(
        &self,
        owner: &str,
        ids: &[(String, String)],
        snap: &PoolSnapshot,
    ) -> Result<Vec<(LpPosition, String)>> {
        let owner: Address = owner.parse()?;
        self.observations.watch_owner(owner);
        let epoch = self.observations.epoch();
        let mut out = vec![];
        for (layer, id) in ids {
            let id = U256::from_str_radix(id, 10)?;
            ensure!(
                word_address(
                    self.words_at(snap, self.manager, "ownerOf(uint256)", &[id])
                        .await?[0]
                ) == owner,
                "NFT not owned by configured wallet"
            );
            let p = self
                .words_at(snap, self.manager, "positions(uint256)", &[id])
                .await?;
            ensure!(p.len() == 12, "invalid NFPM position");
            let (t0, t1) = if self.base < self.quote {
                (self.base, self.quote)
            } else {
                (self.quote, self.base)
            };
            ensure!(
                word_address(p[2]) == t0
                    && word_address(p[3]) == t1
                    && p[4] == U256::from(self.cfg.fee),
                "NFT belongs to another pool"
            );
            let a = math::tick_to_price(
                signed_tick(p[5]),
                self.cfg.base_decimals,
                self.cfg.quote_decimals,
                snap.base_is_token0,
            );
            let b = math::tick_to_price(
                signed_tick(p[6]),
                self.cfg.base_decimals,
                self.cfg.quote_decimals,
                snap.base_is_token0,
            );
            let lower = a.min(b);
            let upper = a.max(b);
            let liquidity = p[7].to_string().parse::<f64>()?
                / 10_f64
                    .powf((self.cfg.base_decimals as f64 + self.cfg.quote_decimals as f64) / 2.0);
            let (mut base, mut quote) = math::amounts(liquidity, lower, upper, snap.price);
            let (f0, f1) = self.unclaimed(&p, snap).await?;
            let (fb, fq) = if snap.base_is_token0 {
                (f0, f1)
            } else {
                (f1, f0)
            };
            base += units(fb, self.cfg.base_decimals)?;
            quote += units(fq, self.cfg.quote_decimals)?;
            out.push((
                LpPosition {
                    layer: layer.clone(),
                    token_id: Some(id.to_string()),
                    lower,
                    upper,
                    liquidity,
                    raw_liquidity: p[7].to_string(),
                    unclaimed_base: units(fb, self.cfg.base_decimals)?,
                    unclaimed_quote: units(fq, self.cfg.quote_decimals)?,
                    base,
                    quote,
                },
                format!(
                    "{}:{}:{}:{}:{}:{}:{}",
                    p[7], p[8], p[9], p[5], p[6], p[10], p[11]
                ),
            ));
        }
        self.observations.ensure_epoch(epoch)?;
        self.observations
            .save_positions(epoch, owner, ids, snap, &out);
        Ok(out)
    }
}
pub fn units(raw: U256, decimals: u8) -> Result<f64> {
    Ok(raw.to_string().parse::<f64>()? / 10_f64.powi(decimals as i32))
}
pub fn raw_units(amount: f64, decimals: u8) -> Result<U256> {
    use rust_decimal::{Decimal, prelude::FromPrimitive};
    ensure!(amount.is_finite() && amount >= 0.0, "invalid token amount");
    ensure!(decimals <= 24, "unsupported token decimals");
    let d = Decimal::from_f64(amount).context("token amount overflow")?;
    let scale = Decimal::from_i128_with_scale(10_i128.pow(decimals as u32), 0);
    let s = d
        .checked_mul(scale)
        .context("token amount overflow")?
        .trunc()
        .to_string();
    Ok(U256::from_str_radix(&s, 10)?)
}

pub fn bounded_amount(amount: f64, decimals: u8, balance: U256) -> Result<U256> {
    let requested = raw_units(amount, decimals)?;
    if requested <= balance {
        return Ok(requested);
    }
    // A display f64 converted from a wallet's exact balance can round up a few wei.
    let tolerance = balance / U256::from(1_000_000_000_000u64) + U256::from(2);
    ensure!(
        requested - balance <= tolerance,
        "insufficient token balance"
    );
    Ok(balance)
}
