//! 复用已测试的手续费数量差分/时间加权 APR 算法，Solana 使用独立文件与 position 公钥身份。
use super::{config::Config, dlmm::Snapshot, journal::Positions};
use crate::{
    monitor::performance::{History, Holding, Starts},
    store::Store,
};
use anyhow::Result;
use serde_json::{Value, json};
use std::collections::VecDeque;
pub fn observe(c: &Config, store: &Store, s: &Snapshot) -> Result<Value> {
    observe_at(c, store, s, crate::now_ms())
}
pub fn observe_at(c: &Config, store: &Store, s: &Snapshot, now: u64) -> Result<Value> {
    let identity = json!({"chain_id":"solana-mainnet","pool":c.solana.pool,"manager":"meteora-dlmm","owner":c.solana.owner});
    let mut history = store
        .read::<History>("solana_performance.json")?
        .unwrap_or_default();
    if history.validate().is_err() || history.identity != identity {
        history = History {
            identity: identity.clone(),
            ..Default::default()
        };
    }
    let positions = store
        .read::<Positions>("solana_positions.json")?
        .unwrap_or_default();
    for p in positions.values() {
        history
            .positions
            .entry(p.address.clone())
            .or_insert(Holding {
                since_ms: p.since_ms,
                since_source: "confirmed_position_creation".into(),
                samples: VecDeque::new(),
                reset_reason: None,
            });
    }
    let rows:Vec<_>=s.positions.iter().map(|p|json!({"token_id":p.address,"accounting_revision":p.revision,"unclaimed_base":p.fee_base,"unclaimed_quote":p.fee_quote,"principal_value_usdg":p.base*s.price+p.quote})).collect();
    let mut report = json!({"mode":"live","positions_observed":true,"accounting_identity":identity,"pool":{"time_ms":s.time_ms,"block":s.slot,"block_hash":s.block_hash,"price":s.price},"positions":rows});
    history.observe(
        &mut report,
        now,
        c.strategy.max_data_age_seconds * 1000,
        &Starts::default(),
    )?;
    store.write("solana_performance.json", &history)?;
    // USDG is an old internal field name in the shared calculator; never expose it on Solana.
    fn normalize(v: &mut Value) {
        match v {
            Value::Object(m) => {
                for (old, new) in [
                    ("fees_usdg", "fees_usdc"),
                    ("average_principal_usdg", "average_principal_usdc"),
                    ("principal_value_usdg", "principal_value_usdc"),
                ] {
                    if let Some(x) = m.remove(old) {
                        m.insert(new.into(), x);
                    }
                }
                for x in m.values_mut() {
                    normalize(x);
                }
            }
            Value::Array(a) => {
                for x in a {
                    normalize(x);
                }
            }
            _ => {}
        }
    }
    normalize(&mut report);
    Ok(report)
}
