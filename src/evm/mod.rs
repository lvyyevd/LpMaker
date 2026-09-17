pub mod abi;
pub mod events;
pub mod nonce;
pub mod rpc;
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
}
impl UniswapV3 {
    pub fn new(cfg: LiquidityConfig) -> Result<Self> {
        Ok(Self {
            rpc: Rpc::new(cfg.rpc_url.clone())?,
            archive_rpc: Rpc::new(
                cfg.archive_rpc_url
                    .clone()
                    .unwrap_or_else(|| cfg.rpc_url.clone()),
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
        let count = self
            .rpc
            .words(
                self.manager,
                "balanceOf(address)",
                &[address_word(owner)],
                "latest",
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
                    "latest",
                )
                .await?[0];
            let p = self.position_words(id, "latest").await?;
            let t0 = word_address(p[2]);
            let t1 = word_address(p[3]);
            if ((t0 == self.base && t1 == self.quote) || (t0 == self.quote && t1 == self.base))
                && p[4] == U256::from(self.cfg.fee)
                && p[7] > U256::ZERO
            {
                ids.push(id.to_string());
            }
        }
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
    async fn unclaimed(&self, position: &[U256], tick: i32, block: &str) -> Result<(U256, U256)> {
        let lower = self
            .rpc
            .words(
                self.pool,
                "ticks(int24)",
                &[tick_word(signed_tick(position[5]))],
                block,
            )
            .await?;
        let upper = self
            .rpc
            .words(
                self.pool,
                "ticks(int24)",
                &[tick_word(signed_tick(position[6]))],
                block,
            )
            .await?;
        ensure!(lower.len() >= 4 && upper.len() >= 4, "tick ABI mismatch");
        let mut out = [U256::ZERO; 2];
        for i in 0..2 {
            let global = self
                .rpc
                .words(
                    self.pool,
                    if i == 0 {
                        "feeGrowthGlobal0X128()"
                    } else {
                        "feeGrowthGlobal1X128()"
                    },
                    &[],
                    block,
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
        Ok(())
    }
    async fn snapshot(&self) -> Result<PoolSnapshot> {
        let number = self
            .rpc
            .block_number()
            .await?
            .saturating_sub(self.cfg.confirmations);
        let tag = format!("0x{number:x}");
        let header = self
            .rpc
            .request("eth_getBlockByNumber", json!([tag, false]))
            .await?;
        let slot = self.rpc.words(self.pool, "slot0()", &[], &tag).await?;
        ensure!(
            slot.len() == 7 && slot[6] != U256::ZERO,
            "uninitialized/locked pool"
        );
        let l = self.rpc.words(self.pool, "liquidity()", &[], &tag).await?[0];
        let spacing = signed_tick(
            self.rpc
                .words(self.pool, "tickSpacing()", &[], &tag)
                .await?[0],
        );
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
    async fn positions(&self, owner: &str, ids: &[(String, String)]) -> Result<Vec<LpPosition>> {
        let owner: Address = owner.parse()?;
        let snap = self.snapshot().await?;
        let block = format!("0x{:x}", snap.block);
        let mut out = vec![];
        for (layer, id) in ids {
            let id = U256::from_str_radix(id, 10)?;
            ensure!(
                word_address(
                    self.rpc
                        .words(self.manager, "ownerOf(uint256)", &[id], &block)
                        .await?[0]
                ) == owner,
                "NFT not owned by configured wallet"
            );
            let p = self.position_words(id, &block).await?;
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
            let (f0, f1) = self.unclaimed(&p, snap.tick, &block).await?;
            let (fb, fq) = if snap.base_is_token0 {
                (f0, f1)
            } else {
                (f1, f0)
            };
            base += units(fb, self.cfg.base_decimals)?;
            quote += units(fq, self.cfg.quote_decimals)?;
            out.push(LpPosition {
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
            });
        }
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
