use anyhow::{Result, ensure};

pub fn amounts(liquidity: f64, lower: f64, upper: f64, price: f64) -> (f64, f64) {
    let p = price.clamp(lower, upper).sqrt();
    (
        liquidity * (1.0 / p - 1.0 / upper.sqrt()),
        liquidity * (p - lower.sqrt()),
    )
}
pub fn liquidity_for_value(value: f64, lower: f64, upper: f64, price: f64) -> Result<f64> {
    ensure!(
        [value, lower, upper, price]
            .iter()
            .all(|x| x.is_finite() && *x > 0.0)
            && lower < upper,
        "invalid liquidity inputs"
    );
    let (b, q) = amounts(1.0, lower, upper, price);
    Ok(value / (b * price + q))
}
pub fn range(price: f64, width: f64) -> (f64, f64) {
    (price / (1.0 + width), price * (1.0 + width))
}
pub fn price_to_tick(
    price: f64,
    base_decimals: u8,
    quote_decimals: u8,
    base_is_token0: bool,
) -> Result<f64> {
    ensure!(price.is_finite() && price > 0.0, "invalid price");
    let raw = price * 10_f64.powi(quote_decimals as i32 - base_decimals as i32);
    Ok(if base_is_token0 { raw.ln() } else { -raw.ln() } / 1.0001_f64.ln())
}
pub fn tick_to_price(
    tick: i32,
    base_decimals: u8,
    quote_decimals: u8,
    base_is_token0: bool,
) -> f64 {
    let raw = 1.0001_f64.powi(tick);
    (if base_is_token0 { raw } else { 1.0 / raw })
        * 10_f64.powi(base_decimals as i32 - quote_decimals as i32)
}
pub fn aligned_ticks(
    lower: f64,
    upper: f64,
    spacing: i32,
    bd: u8,
    qd: u8,
    base0: bool,
) -> Result<(i32, i32)> {
    ensure!(spacing > 0 && lower < upper, "invalid tick range");
    let a = price_to_tick(lower, bd, qd, base0)?;
    let b = price_to_tick(upper, bd, qd, base0)?;
    let lo = (a.min(b) / spacing as f64).floor() as i32 * spacing;
    let hi = (a.max(b) / spacing as f64).ceil() as i32 * spacing;
    ensure!(
        lo >= -887272 && hi <= 887272 && lo < hi,
        "ticks outside Uniswap bounds"
    );
    Ok((lo, hi))
}
