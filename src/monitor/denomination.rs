//! 将历史 USDG/WETH 字段转换为通用计价字段。
//! Robinhood 继续附带旧字段供原有日志读取程序使用；Base 不输出误导性的 USDG 字段。
use serde_json::Value;

pub fn normalize(value: &mut Value, keep_legacy: bool) {
    match value {
        Value::Object(map) => {
            for nested in map.values_mut() {
                normalize(nested, keep_legacy);
            }
            for (old, new) in [
                ("principal_value_usdg", "principal_value_quote"),
                ("unclaimed_fees_usdg", "unclaimed_fees_quote"),
                ("fees_usdg", "fees_quote"),
                ("average_principal_usdg", "average_principal_quote"),
                ("volume_usdg", "volume_quote"),
                ("volume_weth", "volume_base"),
            ] {
                if let Some(v) = map.get(old).cloned() {
                    map.insert(new.into(), v);
                }
                if !keep_legacy {
                    map.remove(old);
                }
            }
        }
        Value::Array(rows) => {
            for row in rows {
                normalize(row, keep_legacy);
            }
        }
        _ => {}
    }
}

pub fn value<'a>(row: &'a Value, key: &str, legacy: &str) -> &'a Value {
    row.get(key).unwrap_or(&row[legacy])
}
