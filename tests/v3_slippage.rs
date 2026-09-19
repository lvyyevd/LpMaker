use alloy::primitives::U256;
use lp_maker::liquidity::uniswap_v3::slippage::{
    Range, check_price_move, parse_sqrt, sqrt_at_tick,
};
use serde_json::Value;

/// 固定向量由官方 JS SDK 生成，不由被测 Rust 代码反推预期值。
#[test]
fn matches_official_sdk_including_reversed_token_order_and_rounding() {
    let data: Value = serde_json::from_str(include_str!("fixtures/v3_slippage_sdk.json")).unwrap();
    for v in data["cases"].as_array().unwrap() {
        let n = |key: &str| parse_sqrt(v[key].as_str().unwrap()).unwrap();
        let tick = |key: &str| v[key].as_i64().unwrap() as i32;
        let r = Range::new(tick("lower"), tick("upper")).unwrap();
        let p = n("sqrt");
        let bps = v["bps"].as_u64().unwrap() as u32;
        assert_eq!(sqrt_at_tick(tick("tick")).unwrap(), p);
        assert_eq!(
            r.liquidity(p, n("amount0"), n("amount1")).unwrap(),
            n("liquidity")
        );
        assert_eq!(
            r.mint_minimums(p, n("amount0"), n("amount1"), bps).unwrap(),
            (n("mint0"), n("mint1"))
        );
        assert_eq!(
            r.burn_minimums(p, n("liquidity"), bps).unwrap(),
            (n("burn0"), n("burn1"))
        );
    }
}

#[test]
fn narrow_range_small_move_reproduces_old_revert_and_new_minimum_passes() {
    let r = Range::new(-198300, -197900).unwrap();
    let p = sqrt_at_tick(-198100).unwrap();
    let (a0, a1) = r
        .amounts(p, U256::from(40_000_000_000_000_u64), true)
        .unwrap();
    let mins = r.mint_minimums(p, a0, a1, 30).unwrap();
    // 只移动约 0.05% 的价格；旧的数量打折法在 2% 窄区间也会失败。
    for tick in [-198105, -198095] {
        let moved = sqrt_at_tick(tick).unwrap();
        let used = r
            .amounts(moved, r.liquidity(moved, a0, a1).unwrap(), true)
            .unwrap();
        let old = (
            a0 * U256::from(9970) / U256::from(10000),
            a1 * U256::from(9970) / U256::from(10000),
        );
        assert!(used.0 < old.0 || used.1 < old.1);
        assert!(used.0 >= mins.0 && used.1 >= mins.1);
    }
    let moved = sqrt_at_tick(-198000).unwrap();
    let used = r
        .amounts(moved, r.liquidity(moved, a0, a1).unwrap(), true)
        .unwrap();
    assert!(
        used.0 < mins.0 || used.1 < mins.1,
        "large move must still fail slippage"
    );
}

#[test]
fn burn_uses_raw_liquidity_and_handles_in_range_and_single_sided_positions() {
    let r = Range::new(-200, 200).unwrap();
    let l = U256::from(1_000_000);
    for center in [-400, 0, 400] {
        let p = sqrt_at_tick(center).unwrap();
        let min = r.burn_minimums(p, l, 30).unwrap();
        for movement in -20..=20 {
            let actual = r
                .amounts(sqrt_at_tick(center + movement).unwrap(), l, false)
                .unwrap();
            assert!(actual.0 >= min.0 && actual.1 >= min.1);
        }
        if center < -200 {
            assert_eq!(min.1, U256::ZERO);
        }
        if center > 200 {
            assert_eq!(min.0, U256::ZERO);
        }
    }
}

#[test]
fn rejects_invalid_inputs_and_does_not_chase_price_during_approvals() {
    assert_eq!(sqrt_at_tick(-887272).unwrap(), U256::from(4295128739_u64));
    assert_eq!(
        sqrt_at_tick(887272).unwrap(),
        parse_sqrt("1461446703485210103287273052203988822378723970342").unwrap()
    );
    assert!(sqrt_at_tick(887273).is_err());
    assert!(Range::new(200, -200).is_err());
    let p = sqrt_at_tick(0).unwrap();
    assert!(check_price_move(p, sqrt_at_tick(10).unwrap(), 30).is_ok());
    assert!(check_price_move(p, sqrt_at_tick(40).unwrap(), 30).is_err());
    assert!(check_price_move(p, sqrt_at_tick(-40).unwrap(), 30).is_err());
    let r = Range::new(-200, 200).unwrap();
    assert!(r.mint_minimums(p, U256::ZERO, U256::ZERO, 30).is_err());
    assert!(
        r.mint_minimums(sqrt_at_tick(200).unwrap(), U256::from(1), U256::from(1), 30)
            .is_err()
    );
    assert!(r.burn_minimums(p, U256::MAX, 30).is_err());
}
