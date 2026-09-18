use super::Asset;
use anyhow::{Result, ensure};
use rust_decimal::{Decimal, RoundingStrategy, prelude::FromPrimitive};
use serde_json::{Value, json};
use std::str::FromStr;
pub fn cloid() -> String {
    format!("0x{}", uuid::Uuid::new_v4().simple())
}
/// Opening dust is deferred, never rounded up into additional exposure.
pub fn tradeable(price: &str, size: &str, reduce_only: bool) -> Result<bool> {
    let p = Decimal::from_str(price)?;
    let s = Decimal::from_str(size)?;
    ensure!(
        p > Decimal::ZERO && s >= Decimal::ZERO,
        "invalid hedge amount"
    );
    Ok(s > Decimal::ZERO && (reduce_only || p * s >= Decimal::from(10)))
}
pub fn validate_price(s: &str, sz_decimals: u32) -> Result<Decimal> {
    let d = Decimal::from_str(s)?.normalize();
    ensure!(
        d > Decimal::ZERO && sz_decimals <= 6,
        "invalid price/asset decimals"
    );
    ensure!(d.scale() <= 6 - sz_decimals, "price exceeds tick decimals");
    let significant = d.mantissa().unsigned_abs().to_string().len();
    ensure!(
        d.scale() == 0 || significant <= 5,
        "price exceeds five significant figures"
    );
    Ok(d)
}
#[allow(clippy::too_many_arguments)] // Typed wire boundary; all fields are validated.
pub fn wire(
    id: u32,
    a: &Asset,
    buy: bool,
    price: &str,
    size: &str,
    tif: &str,
    reduce: bool,
    cloid: Option<&str>,
) -> Result<Value> {
    let p = validate_price(price, a.sz_decimals)?;
    let s = Decimal::from_str(size)?.normalize();
    ensure!(
        s > Decimal::ZERO && s.scale() <= a.sz_decimals,
        "size exceeds lot precision or is zero"
    );
    ensure!(
        matches!(tif, "Alo" | "Ioc" | "Gtc"),
        "unknown time in force"
    );
    ensure!(
        reduce || p * s >= Decimal::from(10),
        "order below $10 minimum notional"
    );
    let mut v = json!({"a":id,"b":buy,"p":p.to_string(),"s":s.to_string(),"r":reduce,"t":{"limit":{"tif":tif}}});
    if let Some(c) = cloid {
        ensure!(
            c.len() == 34 && c.starts_with("0x") && hex::decode(&c[2..]).is_ok(),
            "invalid client order id"
        );
        v["c"] = json!(c);
    }
    Ok(v)
}
pub fn quantity(x: f64, decimals: u32) -> Result<String> {
    ensure!(x.is_finite() && x >= 0.0, "invalid quantity");
    Ok(Decimal::from_f64(x)
        .ok_or_else(|| anyhow::anyhow!("quantity overflow"))?
        .round_dp_with_strategy(decimals, RoundingStrategy::ToZero)
        .normalize()
        .to_string())
}
pub fn price(x: f64, sz_decimals: u32, round_up: bool) -> Result<String> {
    ensure!(
        x.is_finite() && x > 0.0 && sz_decimals <= 6,
        "invalid price"
    );
    let dp = (4 - x.log10().floor() as i32).max(0) as u32;
    let p = Decimal::from_f64(x)
        .ok_or_else(|| anyhow::anyhow!("price overflow"))?
        .round_dp_with_strategy(
            dp.min(6 - sz_decimals),
            if round_up {
                RoundingStrategy::ToPositiveInfinity
            } else {
                RoundingStrategy::ToNegativeInfinity
            },
        )
        .normalize();
    validate_price(&p.to_string(), sz_decimals)?;
    Ok(p.to_string())
}
