//! Mode-aware collateral observations. Raw exchange fields are preserved for auditing.
use super::{Client, num};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Collateral {
    pub account_mode: String,
    pub source: String,
    pub coin: String,
    pub equity_usdc: f64,
    pub available_buy_usdc: f64,
    pub available_short_usdc: f64,
}
pub fn collateral(account: &Value) -> Result<Collateral> {
    serde_json::from_value(account["lpMakerCollateral"].clone())
        .context("missing mode-aware collateral observation")
}

impl Client {
    /// Native perp positions remain authoritative even when their balance fields are not.
    pub async fn perp_account(&self) -> Result<Value> {
        self.info(json!({"type":"clearinghouseState","user":self.user()?}))
            .await
    }
    pub async fn collateral_account(&self) -> Result<Value> {
        let mode = self
            .info(json!({"type":"userAbstraction","user":self.user()?}))
            .await?;
        let mode = mode.as_str().context("invalid Hyperliquid account mode")?;
        validate_mode(mode)?;
        let (perp, spot, active) = if mode == "unifiedAccount" {
            let (perp, spot, active) = tokio::try_join!(
                self.perp_account(),
                self.info(json!({"type":"spotClearinghouseState","user":self.user()?})),
                self.active_asset(&self.cfg.hedge_coin),
            )?;
            (perp, Some(spot), Some(active))
        } else {
            (self.perp_account().await?, None, None)
        };
        // Do not combine a standard balance with a unified balance during a mode switch.
        let after = self
            .info(json!({"type":"userAbstraction","user":self.user()?}))
            .await?;
        ensure!(
            after == mode,
            "Hyperliquid account mode changed during observation; reconcile again"
        );
        let snapshot = normalize(mode, &self.cfg.hedge_coin, perp, spot, active)?;
        tracing::debug!(collateral=%snapshot["lpMakerCollateral"], "Hyperliquid collateral reconciled");
        Ok(snapshot)
    }
}

fn validate_mode(mode: &str) -> Result<()> {
    match mode {
        "unifiedAccount" | "disabled" | "default" | "dexAbstraction" => Ok(()),
        "portfolioMargin" => bail!(
            "Hyperliquid portfolioMargin borrowing/collateral valuation is unsupported; refusing to treat buying power as cash"
        ),
        _ => bail!("unsupported Hyperliquid account mode: {mode}"),
    }
}

pub(crate) fn normalize(
    mode: &str,
    coin: &str,
    mut perp: Value,
    spot: Option<Value>,
    active: Option<Value>,
) -> Result<Value> {
    validate_mode(mode)?;
    ensure!(perp["assetPositions"].is_array(), "missing perp positions");
    let (equity, buy, short, source) = if mode == "unifiedAccount" {
        let spot = spot
            .as_ref()
            .context("unified account requires spot state")?;
        let balances = spot["balances"]
            .as_array()
            .context("missing spot balances")?;
        let mut usdc = None;
        for b in balances {
            let token = b["token"].as_u64().context("invalid spot token id")?;
            if token == 0 || b["coin"] == "USDC" {
                ensure!(
                    token == 0 && b["coin"] == "USDC" && usdc.is_none(),
                    "ambiguous USDC collateral balance"
                );
                let total = num(&b["total"])?;
                let hold = num(&b["hold"])?;
                ensure!(hold >= 0.0, "negative collateral hold");
                usdc = Some((total, (total - hold).max(0.0)));
            }
        }
        let (total, free) = usdc.unwrap_or((0.0, 0.0));
        let active = active
            .as_ref()
            .context("unified account requires active asset data")?;
        ensure!(
            active["coin"] == coin,
            "collateral observation coin mismatch"
        );
        let amounts = active["availableToTrade"]
            .as_array()
            .context("missing availableToTrade")?;
        ensure!(amounts.len() == 2, "invalid availableToTrade directions");
        let buy = num(&amounts[0])?;
        let short = num(&amounts[1])?;
        ensure!(buy >= 0.0 && short >= 0.0, "negative available collateral");
        // Unified balances already represent the shared collateral ledger. Do not add the
        // legacy perp accountValue or position PnL again, or count USDH/HYPE as USDC.
        (
            total,
            buy.min(free),
            short.min(free),
            "spot USDC total; min(activeAssetData availableToTrade[buy,sell], USDC total-hold)",
        )
    } else {
        let equity = num(&perp["marginSummary"]["accountValue"])?;
        let free = num(&perp["withdrawable"])?;
        ensure!(free >= 0.0, "negative withdrawable collateral");
        (
            equity,
            free,
            free,
            "native clearinghouseState accountValue/withdrawable",
        )
    };
    let collateral = Collateral {
        account_mode: mode.into(),
        source: source.into(),
        coin: coin.into(),
        equity_usdc: equity,
        available_buy_usdc: buy,
        available_short_usdc: short,
    };
    perp["lpMakerCollateral"] = serde_json::to_value(collateral)?;
    perp["accountObservedMs"] = json!(crate::now_ms());
    if let Some(spot) = spot {
        perp["spotClearinghouseState"] = spot;
    }
    if let Some(active) = active {
        perp["activeAssetData"] = active;
    }
    Ok(perp)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn perp() -> Value {
        json!({"marginSummary":{"accountValue":"0"},"withdrawable":"0","assetPositions":[]})
    }
    fn spot() -> Value {
        json!({"balances":[{"coin":"USDC","token":0,"total":"79.6","hold":"0"}]})
    }
    fn active(buy: &str, sell: &str) -> Value {
        json!({"coin":"ETH","availableToTrade":[buy,sell]})
    }
    #[test]
    fn unified_collateral_uses_real_cash_not_zero_legacy_perp_balance() {
        let v = normalize(
            "unifiedAccount",
            "ETH",
            perp(),
            Some(spot()),
            Some(active("79.6", "79.6")),
        )
        .unwrap();
        let c = collateral(&v).unwrap();
        assert_eq!(c.equity_usdc, 79.6);
        assert_eq!(c.available_short_usdc, 79.6);
        assert_eq!(v["marginSummary"]["accountValue"], "0");
        assert_eq!(v["withdrawable"], "0"); // Raw audit data was not rewritten.
    }
    #[test]
    fn directions_holds_and_non_usdc_assets_are_not_spendable_short_margin() {
        let mut s = spot();
        s["balances"][0]["hold"] = json!("50");
        s["balances"]
            .as_array_mut()
            .unwrap()
            .push(json!({"coin":"USDH","token":360,"total":"1000","hold":"0"}));
        let v = normalize(
            "unifiedAccount",
            "ETH",
            perp(),
            Some(s),
            Some(active("75", "12")),
        )
        .unwrap();
        let c = collateral(&v).unwrap();
        assert_eq!(c.equity_usdc, 79.6);
        assert!((c.available_buy_usdc - 29.6).abs() < 1e-8);
        assert_eq!(c.available_short_usdc, 12.0);
    }
    #[test]
    fn unified_does_not_add_legacy_equity_or_pnl_twice() {
        let mut p = perp();
        p["marginSummary"]["accountValue"] = json!("45");
        p["assetPositions"] = json!([{"position":{"coin":"ETH","szi":"-0.02","unrealizedPnl":"5","marginUsed":"20"}}]);
        let v = normalize(
            "unifiedAccount",
            "ETH",
            p,
            Some(spot()),
            Some(active("60", "55")),
        )
        .unwrap();
        assert_eq!(collateral(&v).unwrap().equity_usdc, 79.6);
        assert_eq!(super::super::position_size(&v, "ETH").unwrap(), -0.02);
    }
    #[test]
    fn standard_and_dex_abstraction_keep_native_balance_without_adding_spot() {
        for mode in ["disabled", "default", "dexAbstraction"] {
            let mut p = perp();
            p["marginSummary"]["accountValue"] = json!("60");
            p["withdrawable"] = json!("40");
            let c = collateral(&normalize(mode, "ETH", p, Some(spot()), None).unwrap()).unwrap();
            assert_eq!(c.equity_usdc, 60.0);
            assert_eq!(c.available_short_usdc, 40.0);
        }
    }
    #[test]
    fn absent_usdc_is_zero_but_missing_or_corrupt_observations_are_errors() {
        let empty = json!({"balances":[]});
        let c = collateral(
            &normalize(
                "unifiedAccount",
                "ETH",
                perp(),
                Some(empty),
                Some(active("79.6", "79.6")),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(c.equity_usdc, 0.0);
        assert_eq!(c.available_short_usdc, 0.0);
        for s in [
            Value::Null,
            json!({"balances":[{"coin":"USDC","token":360,"total":"79.6","hold":"0"}]}),
            json!({"balances":[{"coin":"USDC","token":0,"total":"NaN","hold":"0"}]}),
            json!({"balances":[{"coin":"USDC","token":0,"total":"79.6","hold":"-1"}]}),
        ] {
            assert!(
                normalize(
                    "unifiedAccount",
                    "ETH",
                    perp(),
                    Some(s),
                    Some(active("79.6", "79.6"))
                )
                .is_err()
            );
        }
        assert!(normalize("unifiedAccount", "ETH", perp(), Some(spot()), None).is_err());
        assert!(
            normalize(
                "unifiedAccount",
                "ETH",
                perp(),
                Some(spot()),
                Some(active("79.6", "NaN"))
            )
            .is_err()
        );
        assert!(
            normalize(
                "unifiedAccount",
                "ETH",
                perp(),
                Some(spot()),
                Some(json!({"coin":"ETH","availableToTrade":["79.6"]}))
            )
            .is_err()
        );
        assert!(
            normalize(
                "unifiedAccount",
                "BTC",
                perp(),
                Some(spot()),
                Some(active("79.6", "79.6"))
            )
            .is_err()
        );
    }
    #[test]
    fn borrowing_and_unknown_modes_never_fall_back_to_standard_or_unified_cash() {
        for mode in ["portfolioMargin", "unknown", ""] {
            assert!(
                normalize(
                    mode,
                    "ETH",
                    perp(),
                    Some(spot()),
                    Some(active("10000", "10000"))
                )
                .is_err()
            );
        }
    }
}
