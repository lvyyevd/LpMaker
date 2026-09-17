pub mod journal;
pub mod orders;
pub mod signing;
pub mod ws;
use crate::{
    config::HyperliquidConfig,
    domain::{Candle, HedgeVenue},
    store::Store,
};
use alloy::{primitives::Address, signers::local::PrivateKeySigner};
use anyhow::{Context, Result, bail, ensure};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Asset {
    pub name: String,
    pub sz_decimals: u32,
    pub max_leverage: u32,
    #[serde(default)]
    pub is_delisted: bool,
}
#[derive(Clone)]
pub struct Client {
    pub cfg: HyperliquidConfig,
    http: reqwest::Client,
    signer: Option<PrivateKeySigner>,
    store: Arc<Store>,
}
impl Client {
    pub fn new(cfg: HyperliquidConfig, store: Arc<Store>) -> Result<Self> {
        Ok(Self {
            cfg,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()?,
            signer: None,
            store,
        })
    }
    pub fn enable_signing(&mut self) -> Result<()> {
        ensure!(
            self.cfg.mainnet == self.cfg.http_url.contains("api.hyperliquid.xyz"),
            "Hyperliquid network/signature domain mismatch"
        );
        ensure!(
            self.cfg.http_url == "https://api.hyperliquid.xyz"
                || self.cfg.http_url == "https://api.hyperliquid-testnet.xyz",
            "live exchange URL must be an official Hyperliquid endpoint"
        );
        let key = std::env::var(&self.cfg.private_key_env)
            .context("missing Hyperliquid signing environment variable")?;
        self.signer = Some(key.parse().context("invalid Hyperliquid signer")?);
        let _: Address = self.user()?.parse()?;
        Ok(())
    }
    pub fn user(&self) -> Result<&str> {
        self.cfg
            .vault
            .as_deref()
            .or(self.cfg.account.as_deref())
            .context("Hyperliquid account owner is required")
    }
    pub async fn info(&self, request: Value) -> Result<Value> {
        let started = std::time::Instant::now();
        tracing::debug!(request_type=%request["type"], "Hyperliquid info request started");
        for attempt in 0..3 {
            let r = self
                .http
                .post(format!("{}/info", self.cfg.http_url))
                .json(&request)
                .send()
                .await?;
            if (r.status() == 429 || r.status().is_server_error()) && attempt < 2 {
                tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
                continue;
            }
            let value = r.error_for_status()?.json().await?;
            tracing::debug!(request_type=%request["type"], elapsed_ms=started.elapsed().as_millis(), "Hyperliquid info response received");
            return Ok(value);
        }
        unreachable!()
    }
    pub async fn assets(&self) -> Result<Vec<Asset>> {
        let v = self.info(json!({"type":"meta"})).await?;
        Ok(serde_json::from_value(v["universe"].clone())?)
    }
    pub async fn asset(&self, coin: &str) -> Result<(u32, Asset)> {
        self.assets()
            .await?
            .into_iter()
            .enumerate()
            .find(|(_, a)| a.name == coin && !a.is_delisted)
            .map(|(i, a)| (i as u32, a))
            .context("unknown/delisted native perpetual asset")
    }
    pub async fn candles(
        &self,
        coin: &str,
        interval: &str,
        start: u64,
        end: u64,
    ) -> Result<Vec<Candle>> {
        let v=self.info(json!({"type":"candleSnapshot","req":{"coin":coin,"interval":interval,"startTime":start,"endTime":end}})).await?;
        let mut out = vec![];
        for row in v.as_array().context("invalid candle response")? {
            out.push(Candle {
                open_ms: row["t"].as_u64().context("candle timestamp")?,
                close_ms: row["T"].as_u64().context("candle end")?,
                open: num(&row["o"])?,
                high: num(&row["h"])?,
                low: num(&row["l"])?,
                close: num(&row["c"])?,
            });
        }
        out.sort_by_key(|c| c.open_ms);
        Ok(out)
    }
    pub async fn book(&self, coin: &str) -> Result<(f64, f64, u64)> {
        let v = self.info(json!({"type":"l2Book","coin":coin})).await?;
        let bid = num(&v["levels"][0][0]["px"])?;
        let ask = num(&v["levels"][1][0]["px"])?;
        ensure!(bid > 0.0 && ask >= bid, "invalid/empty order book");
        Ok((
            bid,
            ask,
            v["time"].as_u64().context("book timestamp missing")?,
        ))
    }
    pub async fn open_orders(&self) -> Result<Value> {
        self.info(json!({"type":"openOrders","user":self.user()?}))
            .await
    }
    pub async fn fills(&self, start: u64) -> Result<Value> {
        self.info(json!({"type":"userFillsByTime","user":self.user()?,"startTime":start}))
            .await
    }
    pub async fn order_status(&self, oid: Value) -> Result<Value> {
        self.info(json!({"type":"orderStatus","user":self.user()?,"oid":oid}))
            .await
    }
    pub async fn funding_history(&self, coin: &str, start: u64, end: u64) -> Result<Value> {
        self.info(json!({"type":"fundingHistory","coin":coin,"startTime":start,"endTime":end}))
            .await
    }
    pub async fn exchange(&self, action: Value) -> Result<Value> {
        tracing::info!(action=%action, "Hyperliquid exchange operation started");
        let signer = self
            .signer
            .as_ref()
            .context("signing disabled; explicit live mode required")?;
        let nonce = self.store.next_nonce()?;
        let expires = nonce + 60_000;
        let vault = self
            .cfg
            .vault
            .as_ref()
            .map(|v| v.parse::<Address>())
            .transpose()?;
        let signature = signing::sign(
            signer,
            &action,
            vault,
            nonce,
            Some(expires),
            self.cfg.mainnet,
        )?;
        let request = json!({"action":action,"nonce":nonce,"signature":signature,"vaultAddress":self.cfg.vault,"expiresAfter":expires});
        self.store
            .begin(json!({"venue":"hyperliquid","request":request,"user":self.user()?}))?;
        journal::prepare(&self.store, &action, self.user()?, nonce)?;
        // Never retry exchange writes automatically. A timeout can mean the order exists.
        let response: Value = self
            .http
            .post(format!("{}/exchange", self.cfg.http_url))
            .json(&request)
            .send()
            .await
            .context("uncertain exchange result; reconcile pending operation")?
            .error_for_status()?
            .json()
            .await?;
        ensure!(
            response["status"] == "ok" || response["status"] == "err",
            "unknown exchange envelope; pending operation retained"
        );
        if response["status"] == "ok" {
            ensure!(
                matches!(
                    response["response"]["type"].as_str(),
                    Some("default" | "order" | "cancel")
                ),
                "unknown exchange response type; pending operation retained"
            );
        }
        journal::acknowledged(&self.store, &action, &response)?;
        self.store.finish(&response)?;
        validate_response(&response)?;
        Ok(response)
    }
    pub async fn leverage(&self, coin: &str, leverage: u32, cross: bool) -> Result<Value> {
        let (id, a) = self.asset(coin).await?;
        ensure!(
            leverage > 0 && leverage <= a.max_leverage && leverage <= 3,
            "invalid leverage (LpMaker cap: 3x)"
        );
        self.exchange(
            json!({"type":"updateLeverage","asset":id,"isCross":cross,"leverage":leverage}),
        )
        .await
    }
    pub async fn isolated_margin(&self, coin: &str, usdc_micros: i64) -> Result<Value> {
        let (id, _) = self.asset(coin).await?;
        self.exchange(
            json!({"type":"updateIsolatedMargin","asset":id,"isBuy":true,"ntli":usdc_micros}),
        )
        .await
    }
    pub async fn cancel_oid(&self, coin: &str, oid: u64) -> Result<Value> {
        let (id, _) = self.asset(coin).await?;
        self.exchange(json!({"type":"cancel","cancels":[{"a":id,"o":oid}]}))
            .await
    }
    pub async fn cancel_all(&self) -> Result<Vec<Value>> {
        let orders = self.open_orders().await?;
        let mut out = vec![];
        for o in orders.as_array().context("open orders")? {
            out.push(
                self.cancel_oid(
                    o["coin"].as_str().context("coin")?,
                    o["oid"].as_u64().context("oid")?,
                )
                .await?,
            );
        }
        Ok(out)
    }
    pub async fn schedule_cancel(&self, time: Option<u64>) -> Result<Value> {
        let a = if let Some(t) = time {
            ensure!(
                t >= crate::now_ms() + 5000,
                "cancel deadline must be at least 5s ahead"
            );
            json!({"type":"scheduleCancel","time":t})
        } else {
            json!({"type":"scheduleCancel"})
        };
        self.exchange(a).await
    }
    pub async fn modify(
        &self,
        coin: &str,
        oid: u64,
        buy: bool,
        price: &str,
        size: &str,
        reduce: bool,
    ) -> Result<Value> {
        let (id, a) = self.asset(coin).await?;
        let o = orders::wire(id, &a, buy, price, size, "Alo", reduce, None)?;
        self.exchange(json!({"type":"batchModify","modifies":[{"oid":oid,"order":o}]}))
            .await
    }
    pub async fn trigger(
        &self,
        coin: &str,
        buy: bool,
        price: &str,
        size: &str,
        trigger: &str,
        tp: bool,
    ) -> Result<Value> {
        let (id, a) = self.asset(coin).await?;
        let mut o = orders::wire(
            id,
            &a,
            buy,
            price,
            size,
            "Gtc",
            true,
            Some(&orders::cloid()),
        )?;
        orders::validate_price(trigger, a.sz_decimals)?;
        o["t"] =
            json!({"trigger":{"isMarket":true,"triggerPx":trigger,"tpsl":if tp{"tp"}else{"sl"}}});
        self.exchange(json!({"type":"order","orders":[o],"grouping":"na"}))
            .await
    }
}
#[async_trait]
impl HedgeVenue for Client {
    async fn market(&self, coin: &str) -> Result<(f64, u64)> {
        let (b, a, t) = self.book(coin).await?;
        Ok(((b + a) / 2.0, t))
    }
    async fn account(&self) -> Result<Value> {
        self.info(json!({"type":"clearinghouseState","user":self.user()?}))
            .await
    }
    #[tracing::instrument(skip(self), fields(operation = "perp_order"))]
    async fn order(
        &self,
        coin: &str,
        buy: bool,
        price: &str,
        size: &str,
        tif: &str,
        reduce_only: bool,
        cloid: &str,
    ) -> Result<Value> {
        let (id, a) = self.asset(coin).await?;
        let o = orders::wire(id, &a, buy, price, size, tif, reduce_only, Some(cloid))?;
        self.exchange(json!({"type":"order","orders":[o],"grouping":"na"}))
            .await
    }
    async fn cancel(&self, coin: &str, cloid: &str) -> Result<Value> {
        let (id, _) = self.asset(coin).await?;
        self.exchange(json!({"type":"cancelByCloid","cancels":[{"asset":id,"cloid":cloid}]}))
            .await
    }
}
pub fn num(v: &Value) -> Result<f64> {
    let n = if let Some(s) = v.as_str() {
        s.parse()?
    } else {
        v.as_f64().context("expected number")?
    };
    ensure!(n.is_finite(), "non-finite exchange number");
    Ok(n)
}
pub fn validate_response(v: &Value) -> Result<()> {
    ensure!(v["status"] == "ok", "exchange rejected: {v}");
    if let Some(statuses) = v["response"]["data"]["statuses"].as_array() {
        for s in statuses {
            if let Some(e) = s.get("error") {
                bail!("exchange action rejected: {e}")
            }
        }
    }
    Ok(())
}
pub fn position_size(account: &Value, coin: &str) -> Result<f64> {
    for p in account["assetPositions"]
        .as_array()
        .context("missing assetPositions")?
    {
        if p["position"]["coin"] == coin {
            return num(&p["position"]["szi"]);
        }
    }
    Ok(0.0)
}
