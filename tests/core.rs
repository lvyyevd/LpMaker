use alloy::{primitives::U256, signers::local::PrivateKeySigner};
use lp_maker::{
    config::Config,
    domain::*,
    engine::Paper,
    hyperliquid::{Asset, orders, signing, validate_response},
    math,
    store::Store,
    strategy::{EntryHistory, Phase, Strategy, indicators},
};
use serde_json::json;
fn config() -> Config {
    Config::load("config/robinhood.toml").unwrap()
}
fn candles(n: usize, step: f64) -> Vec<Candle> {
    (0..n)
        .map(|i| {
            let open = 2500.0 * (1.0 + step).powi(i as i32);
            let close = open * (1.0 + step);
            Candle {
                open_ms: 1_700_000_000_000 + i as u64 * 3_600_000,
                close_ms: 1_700_000_000_000 + (i as u64 + 1) * 3_600_000 - 1,
                open,
                close,
                high: open.max(close) * 1.0001,
                low: open.min(close) * 0.9999,
            }
        })
        .collect()
}
fn frame(c: &Config, bars: Vec<Candle>) -> MarketFrame {
    let last = bars.last().unwrap();
    let now = last.close_ms + 1;
    let p = last.close;
    MarketFrame {
        now_ms: now,
        pool: PoolSnapshot {
            block: 1,
            block_hash: "test".into(),
            time_ms: now,
            price: p,
            tick: 0,
            tick_spacing: 1,
            liquidity: "1".into(),
            sqrt_price_x96: "1".into(),
            base_is_token0: true,
        },
        hedge_price: p,
        hedge_time_ms: now,
        candles: bars,
        portfolio: Paper::new(c).portfolio,
    }
}
#[test]
fn config_is_valid() {
    config().validate().unwrap();
}
#[test]
fn reject_overallocated_budget() {
    let mut c = config();
    c.strategy.reserve += 1.0;
    assert!(c.validate().is_err());
}
#[test]
fn reject_invalid_hysteresis() {
    let mut c = config();
    c.strategy.vol_resume_ratio = 2.0;
    assert!(c.validate().is_err());
}
#[test]
fn reject_duplicate_layers() {
    let mut c = config();
    c.strategy.layers[1].name = c.strategy.layers[0].name.clone();
    assert!(c.validate().is_err());
}
#[test]
fn reject_nan_budget() {
    let mut c = config();
    c.strategy.lp_budget = f64::NAN;
    assert!(c.validate().is_err());
}
#[test]
fn lp_value_and_boundaries() {
    let (lo, hi) = math::range(2500.0, 0.03);
    let l = math::liquidity_for_value(6000.0, lo, hi, 2500.0).unwrap();
    let (x, y) = math::amounts(l, lo, hi, 2500.0);
    assert!((x * 2500.0 + y - 6000.0).abs() < 1e-8);
    assert!((x * 2500.0 - y).abs() < 1e-8);
    assert_eq!(math::amounts(l, lo, hi, lo * 0.9).1, 0.0);
    assert_eq!(math::amounts(l, lo, hi, hi * 1.1).0, 0.0);
}
#[test]
fn lp_delta_equals_base_inventory() {
    let (lo, hi) = math::range(2500.0, 0.03);
    let l = math::liquidity_for_value(6000.0, lo, hi, 2500.0).unwrap();
    let v = |p| {
        let (x, y) = math::amounts(l, lo, hi, p);
        x * p + y
    };
    assert!(((v(2500.01) - v(2499.99)) / 0.02 - math::amounts(l, lo, hi, 2500.0).0).abs() < 1e-6);
}
#[test]
fn tick_alignment_handles_negative_ticks_and_reverse_order() {
    for base0 in [true, false] {
        let (lo, hi) = math::aligned_ticks(2400.0, 2600.0, 10, 18, 6, base0).unwrap();
        assert_eq!(lo % 10, 0);
        assert_eq!(hi % 10, 0);
        let a = math::tick_to_price(lo, 18, 6, base0);
        let b = math::tick_to_price(hi, 18, 6, base0);
        assert!(a.min(b) <= 2400.0);
        assert!(a.max(b) >= 2600.0);
    }
}
#[test]
fn solidity_negative_tick_round_trip() {
    for t in [-887272, -198310, -1, 0, 1, 887272] {
        assert_eq!(
            lp_maker::evm::rpc::signed_tick(lp_maker::evm::rpc::tick_word(t)),
            t
        );
    }
}
#[test]
fn unit_conversion_is_nonnegative_and_floored() {
    assert_eq!(
        lp_maker::evm::raw_units(1.23456789, 6).unwrap(),
        U256::from(1234567)
    );
    assert!(lp_maker::evm::raw_units(-1.0, 18).is_err());
}
// Fixtures from hyperliquid-dex/hyperliquid-python-sdk/tests/signing_test.py.
#[test]
fn official_action_hash_vector() {
    let action = json!({"type":"order","orders":[{"a":4,"b":true,"p":"1670.1","s":"0.0147","r":false,"t":{"limit":{"tif":"Ioc"}}}],"grouping":"na"});
    let h = signing::action_hash(&action, None, 1677777606040, None).unwrap();
    assert_eq!(
        format!("{h:#x}"),
        "0x0fcbeda5ae3c4950a548021552a4fea2226858c4453571bf3f24ba017eac2908"
    );
}
#[test]
fn official_signature_mainnet_and_testnet() {
    let signer: PrivateKeySigner =
        "0x0123456789012345678901234567890123456789012345678901234567890123"
            .parse()
            .unwrap();
    let action = json!({"type":"dummy","num":100000000000u64});
    let m = signing::sign(&signer, &action, None, 0, None, true).unwrap();
    assert_eq!(
        m["r"],
        "0x53749d5b30552aeb2fca34b530185976545bb22d0b3ce6f62e31be961a59298"
    );
    assert_eq!(
        m["s"],
        "0x755c40ba9bf05223521753995abb2f73ab3229be8ec921f350cb447e384d8ed8"
    );
    assert_eq!(m["v"], 27);
    let t = signing::sign(&signer, &action, None, 0, None, false).unwrap();
    assert_eq!(
        t["r"],
        "0x542af61ef1f429707e3c76c5293c80d01f74ef853e34b76efffcb57e574f9510"
    );
    assert_eq!(t["v"], 28);
}
#[test]
fn action_hash_binds_expiry_vault_and_nonce() {
    let a = json!({"type":"scheduleCancel","time":1700000000000u64});
    let h = signing::action_hash(&a, None, 1, None).unwrap();
    assert_ne!(h, signing::action_hash(&a, None, 2, None).unwrap());
    assert_ne!(h, signing::action_hash(&a, None, 1, Some(99)).unwrap());
    assert_ne!(
        h,
        signing::action_hash(&a, Some(alloy::primitives::Address::ZERO), 1, None).unwrap()
    );
}
#[test]
fn prices_enforce_sigfigs_and_decimal_limits() {
    assert!(orders::validate_price("2445.6", 4).is_ok());
    assert!(orders::validate_price("2445.67", 4).is_err());
    assert!(orders::validate_price("0.0012345", 4).is_err());
    assert!(orders::validate_price("100000", 4).is_ok());
    assert!(orders::validate_price("NaN", 4).is_err());
    assert!(orders::validate_price("0", 4).is_err());
}
#[test]
fn rounding_is_side_aware() {
    assert_eq!(orders::price(2445.678, 4, false).unwrap(), "2445.6");
    assert_eq!(orders::price(2445.678, 4, true).unwrap(), "2445.7");
    assert_eq!(orders::quantity(1.234567, 4).unwrap(), "1.2345");
}
#[test]
fn orders_reject_invalid_lots_and_small_notional() {
    let a = Asset {
        name: "ETH".into(),
        sz_decimals: 4,
        max_leverage: 25,
        is_delisted: false,
    };
    assert!(orders::wire(1, &a, false, "2500", "0.0001", "Alo", false, None).is_err());
    assert!(orders::wire(1, &a, true, "2500", "0.0001", "Ioc", true, None).is_ok());
    assert!(orders::wire(1, &a, true, "2500", "1.00001", "Alo", false, None).is_err());
}
#[test]
fn embedded_exchange_errors_are_not_success() {
    assert!(validate_response(&json!({"status":"ok","response":{"type":"order","data":{"statuses":[{"error":"Insufficient margin"}]}}})).is_err());
    assert!(validate_response(&json!({"status":"err","response":"invalid signature"})).is_err());
}
#[test]
fn pending_operations_survive_restart_and_clear() {
    let dir = tempfile::tempdir().unwrap();
    {
        let s = Store::open(dir.path()).unwrap();
        s.begin(json!({"hash":"abc"})).unwrap();
        assert!(s.begin(json!({})).is_err());
    }
    {
        let s = Store::open(dir.path()).unwrap();
        assert_eq!(s.pending().unwrap().unwrap()["hash"], "abc");
        s.finish(json!({"status":"ok"})).unwrap();
        assert!(s.pending().unwrap().is_none());
        s.begin(json!({"next":true})).unwrap();
    }
}
#[test]
fn exclusive_process_lock() {
    let dir = tempfile::tempdir().unwrap();
    let _s = Store::open(dir.path()).unwrap();
    assert!(Store::open(dir.path()).is_err());
}
#[test]
fn monotonic_nonces() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path()).unwrap();
    let a = s.next_nonce().unwrap();
    let b = s.next_nonce().unwrap();
    assert!(b > a);
}
#[test]
fn slow_low_volatility_decline_pauses() {
    let c = config();
    let f = frame(&c, candles(200, -0.001));
    let metrics = indicators::calculate(&f.candles, &c.strategy, f.now_ms).unwrap();
    assert!(metrics.downtrend);
    assert!(metrics.vol_ratio < 1.0);
    let d = Strategy::default().evaluate(&c.strategy, &f);
    assert_eq!(d.state, "Paused");
    assert_eq!(d.lp, LpIntent::ExitToQuote);
}
#[test]
fn fast_drop_pauses_even_before_lower_boundary() {
    let c = config();
    let mut f = frame(&c, candles(200, 0.0001));
    f.pool.price *= 0.98;
    f.hedge_price = f.pool.price;
    let d = Strategy::default().evaluate(&c.strategy, &f);
    assert!(d.reasons.contains(&"fast_drop".into()));
    assert_eq!(d.lp, LpIntent::ExitToQuote);
}
#[test]
fn no_timeout_recenter_during_persistent_decline() {
    let c = config();
    let bars = candles(230, -0.001);
    let mut s = Strategy::default();
    for n in 200..230 {
        let f = frame(&c, bars[..n].to_vec());
        let d = s.evaluate(&c.strategy, &f);
        assert_eq!(d.lp, LpIntent::ExitToQuote);
    }
}
#[test]
fn stale_data_does_not_trigger_price_based_trades() {
    let c = config();
    let mut f = frame(&c, candles(200, 0.001));
    f.pool.time_ms -= 120_000;
    f.portfolio.short_base = 1.0;
    let d = Strategy::default().evaluate(&c.strategy, &f);
    assert_eq!(d.lp, LpIntent::Hold);
    assert_eq!(d.target_short_base, 1.0);
    assert_eq!(d.state, "Paused");
}
#[test]
fn future_or_duplicate_hour_does_not_supply_warmup() {
    let c = config();
    let mut f = frame(&c, candles(200, 0.0001));
    f.candles[50].open_ms += 1;
    assert!(indicators::calculate(&f.candles, &c.strategy, f.now_ms).is_err());
    let f = frame(&c, candles(100, 0.0));
    assert!(indicators::calculate(&f.candles, &c.strategy, f.now_ms).is_err());
}
#[test]
fn first_entry_retries_without_cooldown_or_six_new_healthy_hours() {
    let c = config();
    let f = frame(&c, candles(200, 0.0001));
    let mut s = Strategy {
        phase: Phase::Paused,
        pause_since: f.now_ms,
        ..Default::default()
    };
    let d = s.evaluate(&c.strategy, &f);
    assert_eq!(d.lp, LpIntent::Deploy { fraction: 1.0 });
    assert_eq!(s.phase, Phase::Active);
    assert_eq!(s.healthy_hours, 1);
    // A decision alone is not proof that mint succeeded.
    assert_eq!(s.entry_history, EntryHistory::Initial);
    assert!(d.reasons.iter().any(|r| r.starts_with("initial_entry:")));
}
#[test]
fn first_entry_exemption_preserves_all_market_and_halt_gates() {
    let c = config();
    let healthy = frame(&c, candles(200, 0.0001));
    let mut cases = vec![frame(&c, candles(200, -0.001))];
    let mut fast = healthy.clone();
    fast.pool.price *= 0.98;
    fast.hedge_price = fast.pool.price;
    cases.push(fast);
    let mut basis = healthy.clone();
    basis.hedge_price *= 1.1;
    cases.push(basis);
    let mut stale = healthy.clone();
    stale.pool.time_ms -= 120_000;
    cases.push(stale);
    let mut missing = healthy.clone();
    missing.candles = candles(10, 0.0001);
    cases.push(missing);
    let mut volatile = healthy.clone();
    for (i, bar) in volatile.candles.iter_mut().rev().take(6).enumerate() {
        bar.close *= if i % 2 == 0 { 1.003 } else { 0.997 };
        bar.high = bar.high.max(bar.close);
        bar.low = bar.low.min(bar.close);
    }
    assert!(
        indicators::calculate(&volatile.candles, &c.strategy, volatile.now_ms)
            .unwrap()
            .vol_ratio
            > c.strategy.vol_pause_ratio
    );
    cases.push(volatile);
    for f in cases {
        let mut s = Strategy {
            phase: Phase::Paused,
            pause_since: f.now_ms,
            ..Default::default()
        };
        assert!(!matches!(
            s.evaluate(&c.strategy, &f).lp,
            LpIntent::Deploy { .. }
        ));
        assert_ne!(s.phase, Phase::Active);
    }
    let mut halted = Strategy {
        phase: Phase::Halted,
        ..Default::default()
    };
    assert_eq!(
        halted.evaluate(&c.strategy, &healthy).lp,
        LpIntent::ExitToQuote
    );
    assert_eq!(halted.phase, Phase::Halted);
}
#[test]
fn first_real_lp_consumes_exemption_even_after_exit_and_restart() {
    let c = config();
    let mut f = frame(&c, candles(200, 0.0001));
    let mut s = Strategy::default();
    let mut p = Paper::new(&c);
    p.apply(
        &s.evaluate(&c.strategy, &f),
        &c,
        f.pool.price,
        f.hedge_price,
        f.now_ms,
    )
    .unwrap();
    assert!(!p.portfolio.positions.is_empty());
    f.portfolio = p.portfolio;
    s.evaluate(&c.strategy, &f);
    assert_eq!(s.entry_history, EntryHistory::Established);
    // Closed inventory and a restart must not make the account "initial" again.
    f.portfolio = Paper::new(&c).portfolio;
    s.observe_lp(false);
    s.phase = Phase::Paused;
    s.pause_since = f.now_ms;
    s.healthy_hours = c.strategy.resume_healthy_hours;
    let mut restored: Strategy = serde_json::from_value(serde_json::to_value(s).unwrap()).unwrap();
    let d = restored.evaluate(&c.strategy, &f);
    assert_eq!(d.lp, LpIntent::ExitToQuote);
    assert!(
        d.reasons
            .iter()
            .any(|r| r.contains("cooldown_remaining_seconds=21600"))
    );
    assert_eq!(restored.entry_history, EntryHistory::Established);
}
#[test]
fn legacy_or_established_warmup_cannot_rearm_first_entry() {
    let c = config();
    let f = frame(&c, candles(200, 0.0001));
    for history in [EntryHistory::LegacyUnknown, EntryHistory::Established] {
        let mut s = Strategy {
            entry_history: history,
            ..Default::default()
        };
        assert_eq!(s.evaluate(&c.strategy, &f).lp, LpIntent::ExitToQuote);
        assert_eq!(s.phase, Phase::Paused);
    }
}
#[test]
fn repeated_polling_does_not_speed_up_recovery() {
    let c = config();
    let f = frame(&c, candles(200, 0.0001));
    let mut s = Strategy {
        entry_history: EntryHistory::Established,
        phase: Phase::Paused,
        pause_since: f.now_ms - 10 * 3_600_000,
        ..Default::default()
    };
    for _ in 0..100 {
        s.evaluate(&c.strategy, &f);
    }
    assert_eq!(s.healthy_hours, 1);
    assert_eq!(s.phase, Phase::Paused);
}
#[test]
fn recovery_is_fractional_and_requires_completed_hours() {
    let c = config();
    let bars = candles(210, 0.0001);
    let initial = frame(&c, bars[..200].to_vec());
    let mut s = Strategy {
        entry_history: EntryHistory::Established,
        phase: Phase::Paused,
        pause_since: initial.now_ms - 10 * 3_600_000,
        ..Default::default()
    };
    for n in 200..205 {
        assert_eq!(
            s.evaluate(&c.strategy, &frame(&c, bars[..n].to_vec())).lp,
            LpIntent::ExitToQuote
        );
    }
    let d = s.evaluate(&c.strategy, &frame(&c, bars[..205].to_vec()));
    assert_eq!(d.lp, LpIntent::Deploy { fraction: 0.25 });
    assert_eq!(s.phase, Phase::Recovering);
}
#[test]
fn drawdown_stop_is_latched() {
    let c = config();
    let mut f = frame(&c, candles(200, 0.0));
    f.portfolio.wallet_quote -= 600.0;
    let mut s = Strategy::default();
    assert_eq!(s.evaluate(&c.strategy, &f).state, "Halted");
    f.portfolio.wallet_quote += 2000.0;
    assert_eq!(s.evaluate(&c.strategy, &f).state, "Halted");
}
#[test]
fn filled_short_quantity_is_not_tripled_by_leverage() {
    let mut c = config();
    c.strategy.fast_drop_1h = 0.5;
    let mut f = frame(&c, candles(200, 0.0));
    let (lo, hi) = math::range(2600.0, 0.02);
    let l = math::liquidity_for_value(2000.0, lo, hi, 2600.0).unwrap();
    let (b, q) = math::amounts(l, lo, hi, f.pool.price);
    f.portfolio.wallet_quote -= 2000.0;
    f.portfolio.positions.push(LpPosition {
        layer: "satellite".into(),
        token_id: None,
        lower: lo,
        upper: hi,
        liquidity: l,
        raw_liquidity: "paper".into(),
        unclaimed_base: 0.0,
        unclaimed_quote: 0.0,
        base: b,
        quote: q,
    });
    let mut s = Strategy {
        phase: Phase::Active,
        ..Default::default()
    };
    let d = s.evaluate(&c.strategy, &f);
    assert!((d.target_short_base - b).abs() < 1e-10);
    assert!(d.emergency);
}
#[test]
fn paper_maker_order_is_not_an_immediate_fill() {
    let c = config();
    let mut p = Paper::new(&c);
    p.mark(2500.0, 2500.0, 1000, &c);
    let d = Decision {
        state: "Active".into(),
        reasons: vec![],
        lp: LpIntent::Hold,
        target_short_base: 1.0,
        emergency: false,
    };
    p.apply(&d, &c, 2500.0, 2500.0, 1000).unwrap();
    assert_eq!(p.portfolio.short_base, 0.0);
    p.mark(2500.0, 2500.0, 1001, &c);
    assert_eq!(p.portfolio.short_base, 0.0);
    p.mark(2501.0, 2501.0, 1002, &c);
    assert_eq!(p.portfolio.short_base, 1.0);
}
#[test]
fn cash_exit_removes_eth_and_short_together_in_paper() {
    let c = config();
    let mut p = Paper::new(&c);
    p.portfolio.wallet_base = 1.0;
    p.portfolio.wallet_quote -= 2500.0;
    p.portfolio.short_base = 1.0;
    p.last_hedge_price = 2500.0;
    let d = Decision {
        state: "Paused".into(),
        reasons: vec![],
        lp: LpIntent::ExitToQuote,
        target_short_base: 1.0,
        emergency: true,
    };
    p.apply(&d, &c, 2500.0, 2500.0, 1).unwrap();
    assert_eq!(p.portfolio.base(), 0.0);
    assert_eq!(p.portfolio.short_base, 0.0);
    assert!(p.portfolio.equity(2500.0) < 10000.0);
}

#[test]
fn readonly_status_can_coexist_with_executor_but_cannot_write() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path()).unwrap();
    s.write("strategy.json", &json!({"phase":"Paused"}))
        .unwrap();
    let r = Store::readonly(dir.path());
    assert_eq!(
        r.read::<serde_json::Value>("strategy.json")
            .unwrap()
            .unwrap()["phase"],
        "Paused"
    );
    assert!(r.write("strategy.json", &json!({})).is_err());
}
#[test]
fn paper_recovery_adds_liquidity_without_recentering() {
    let c = config();
    let mut p = Paper::new(&c);
    p.mark(2500.0, 2500.0, 1, &c);
    let mut d = Decision {
        state: "Recovering".into(),
        reasons: vec![],
        lp: LpIntent::Deploy { fraction: 0.25 },
        target_short_base: 0.0,
        emergency: false,
    };
    p.apply(&d, &c, 2500.0, 2500.0, 1).unwrap();
    let before = p.portfolio.positions.clone();
    d.lp = LpIntent::Deploy { fraction: 0.5 };
    p.mark(2501.0, 2501.0, 2, &c);
    p.apply(&d, &c, 2501.0, 2501.0, 2).unwrap();
    for (old, new) in before.iter().zip(&p.portfolio.positions) {
        assert_eq!(old.lower, new.lower);
        assert_eq!(old.upper, new.upper);
        assert!(new.liquidity > old.liquidity);
    }
    assert!(p.portfolio.wallet_quote >= 0.0);
}
#[test]
fn recovery_counters_reset_across_observation_gaps() {
    let c = config();
    let bars = candles(220, 0.0001);
    let f = frame(&c, bars[..200].to_vec());
    let mut s = Strategy {
        entry_history: EntryHistory::Established,
        phase: Phase::Paused,
        pause_since: f.now_ms - 20 * 3_600_000,
        ..Default::default()
    };
    for n in 200..204 {
        s.evaluate(&c.strategy, &frame(&c, bars[..n].to_vec()));
    }
    assert_eq!(s.healthy_hours, 4);
    s.evaluate(&c.strategy, &frame(&c, bars[..210].to_vec()));
    assert_eq!(s.healthy_hours, 1);
    assert_eq!(s.phase, Phase::Paused);
}
#[test]
fn invalid_candle_times_return_error_instead_of_panicking() {
    let c = config();
    let mut f = frame(&c, candles(200, 0.0));
    f.candles[150].close_ms = 0;
    assert!(indicators::calculate(&f.candles, &c.strategy, f.now_ms).is_err());
}
#[test]
fn volatility_shock_pauses_even_with_flat_overall_trend() {
    let c = config();
    let mut rows = candles(200, 0.00001);
    for (i, r) in rows.iter_mut().enumerate().skip(194) {
        r.close = if i % 2 == 0 { 2520.0 } else { 2490.0 };
        r.high = r.open.max(r.close) * 1.0001;
        r.low = r.open.min(r.close) * 0.9999;
    }
    let f = frame(&c, rows);
    let d = Strategy::default().evaluate(&c.strategy, &f);
    assert!(d.reasons.contains(&"volatility_spike".into()));
    assert_eq!(d.lp, LpIntent::ExitToQuote);
}
#[test]
fn pool_basis_divergence_blocks_deployment() {
    let c = config();
    let mut f = frame(&c, candles(200, 0.0001));
    f.pool.price *= 1.02;
    let d = Strategy::default().evaluate(&c.strategy, &f);
    assert!(d.reasons.contains(&"pool_perp_basis_limit".into()));
    assert_eq!(d.lp, LpIntent::ExitToQuote);
}
#[test]
fn decode_swap_preserves_signed_amounts_and_log_identity() {
    use alloy::primitives::{I256, keccak256};
    let mut data = vec![];
    for v in [
        I256::try_from(-100).unwrap().into_raw(),
        U256::from(245),
        U256::from(1),
        U256::from(2),
        lp_maker::evm::rpc::tick_word(-198310),
    ] {
        data.extend(v.to_be_bytes::<32>());
    }
    let log = json!({"topics":[format!("{:#x}",keccak256("Swap(address,address,int256,int256,uint160,uint128,int24)"))],"data":format!("0x{}",hex::encode(data)),"blockHash":"abc","logIndex":"0x1"});
    let e = lp_maker::evm::events::decode(&log).unwrap();
    assert_eq!(e["kind"], "Swap");
    assert_eq!(e["fields"]["amount0"], "-100");
    assert_eq!(e["fields"]["tick"], -198310);
    assert_eq!(e["blockHash"], "abc");
}
#[test]
fn stale_paper_order_is_replaced_when_target_changes() {
    let c = config();
    let mut p = Paper::new(&c);
    p.mark(2500.0, 2500.0, 1000, &c);
    let mut d = Decision {
        state: "Active".into(),
        reasons: vec![],
        lp: LpIntent::Hold,
        target_short_base: 1.0,
        emergency: false,
    };
    p.apply(&d, &c, 2500.0, 2500.0, 1000).unwrap();
    d.target_short_base = 0.0;
    p.apply(&d, &c, 2500.0, 2500.0, 1001).unwrap();
    assert!(p.pending.is_none());
    p.mark(2600.0, 2600.0, 1002, &c);
    assert_eq!(p.portfolio.short_base, 0.0);
}

#[test]
fn cloned_clients_cannot_reserve_duplicate_nonces() {
    let dir = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(Store::open(dir.path()).unwrap());
    let threads = (0..12)
        .map(|_| {
            let s = store.clone();
            std::thread::spawn(move || s.next_nonce().unwrap())
        })
        .collect::<Vec<_>>();
    let values = threads
        .into_iter()
        .map(|t| t.join().unwrap())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(values.len(), 12);
}

#[test]
fn transaction_abi_selectors_match_deployed_uniswap_interfaces() {
    use alloy::sol_types::SolCall;
    use lp_maker::evm::abi::{INfpm, IRouter02};
    assert_eq!(hex::encode(INfpm::mintCall::SELECTOR), "88316456");
    assert_eq!(
        hex::encode(INfpm::increaseLiquidityCall::SELECTOR),
        "219f5d17"
    );
    assert_eq!(hex::encode(INfpm::collectCall::SELECTOR), "fc6f7865");
    assert_eq!(
        hex::encode(IRouter02::exactInputSingleCall::SELECTOR),
        "04e45aaf"
    );
    assert_eq!(hex::encode(IRouter02::multicallCall::SELECTOR), "5ae401dc");
}

#[test]
fn rebuilding_one_layer_keeps_other_layer_hedged() {
    let c = config();
    let mut p = Paper::new(&c).portfolio;
    p.positions.push(LpPosition {
        layer: "core".into(),
        token_id: None,
        lower: 2600.0,
        upper: 3000.0,
        liquidity: 1.0,
        raw_liquidity: "1".into(),
        unclaimed_base: 0.0,
        unclaimed_quote: 0.0,
        base: 2.0,
        quote: 0.0,
    });
    p.positions.push(LpPosition {
        layer: "satellite".into(),
        token_id: None,
        lower: 2450.0,
        upper: 2550.0,
        liquidity: 1.0,
        raw_liquidity: "1".into(),
        unclaimed_base: 0.0,
        unclaimed_quote: 0.0,
        base: 0.4,
        quote: 1000.0,
    });
    assert_eq!(
        lp_maker::engine::inventory_hedge_target(&c.strategy, &p, 2500.0),
        2.0
    );
}
#[test]
fn all_balance_swap_handles_float_round_trip_but_rejects_real_overdraw() {
    let balance = U256::from_str_radix("1234567890123456789", 10).unwrap();
    let display = lp_maker::evm::units(balance, 18).unwrap();
    let raw = lp_maker::evm::bounded_amount(display, 18, balance).unwrap();
    assert!(raw <= balance);
    assert!(lp_maker::evm::bounded_amount(display + 0.01, 18, balance).is_err());
    assert!(lp_maker::evm::raw_units(1.0, 255).is_err());
}
