use alloy::primitives::{Address, U256, keccak256};
use lp_maker::{
    config::Config,
    domain::PoolSnapshot,
    evm::{
        UniswapV3,
        rpc::{address_word, tick_word},
    },
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

#[tokio::test]
async fn accounting_observations_pin_enumeration_fees_and_revision_to_one_block() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut cfg = Config::load("config/robinhood.toml").unwrap().liquidity;
    cfg.rpc_url = format!("http://{}", listener.local_addr().unwrap());
    let base: Address = cfg.base_token.parse().unwrap();
    let quote: Address = cfg.quote_token.parse().unwrap();
    let owner: Address = "0x0000000000000000000000000000000000000001"
        .parse()
        .unwrap();
    let q128 = U256::from(1) << 128;
    let position = vec![
        U256::ZERO,
        U256::ZERO,
        address_word(base),
        address_word(quote),
        U256::from(cfg.fee),
        tick_word(-200000),
        tick_word(-190000),
        U256::from(1_000_000_000_000_u64),
        U256::ZERO,
        U256::ZERO,
        U256::from(2),
        U256::from(3),
    ];
    let server = tokio::spawn(async move {
        for _ in 0..9 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut data = Vec::new();
            let (headers, body_len) = loop {
                let mut part = [0_u8; 4096];
                let count = socket.read(&mut part).await.unwrap();
                assert!(count > 0);
                data.extend_from_slice(&part[..count]);
                if let Some(end) = data.windows(4).position(|p| p == b"\r\n\r\n") {
                    let text = String::from_utf8_lossy(&data[..end]);
                    let len = text
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|n| n.trim().parse::<usize>().unwrap())
                        })
                        .unwrap();
                    break (end + 4, len);
                }
            };
            while data.len() < headers + body_len {
                let mut part = [0_u8; 4096];
                let count = socket.read(&mut part).await.unwrap();
                assert!(count > 0);
                data.extend_from_slice(&part[..count]);
            }
            let request: Value =
                serde_json::from_slice(&data[headers..headers + body_len]).unwrap();
            assert_eq!(request["method"], "eth_call");
            assert_eq!(request["params"][1], "0x2a");
            let input = request["params"][0]["data"].as_str().unwrap();
            let selector = |name: &str| format!("0x{}", hex::encode(&keccak256(name)[..4]));
            let words = if input.starts_with(&selector("balanceOf(address)")) {
                vec![U256::from(1)]
            } else if input.starts_with(&selector("tokenOfOwnerByIndex(address,uint256)")) {
                vec![U256::from(7)]
            } else if input.starts_with(&selector("positions(uint256)")) {
                position.clone()
            } else if input.starts_with(&selector("ownerOf(uint256)")) {
                vec![address_word(owner)]
            } else if input.starts_with(&selector("ticks(int24)")) {
                vec![U256::ZERO; 8]
            } else if input.starts_with(&selector("feeGrowthGlobal0X128()")) {
                vec![q128]
            } else if input.starts_with(&selector("feeGrowthGlobal1X128()")) {
                vec![q128 / U256::from(1_000_000_000)]
            } else {
                panic!("unexpected read method {input}")
            };
            let result = format!(
                "0x{}",
                words
                    .iter()
                    .map(|w| format!("{w:064x}"))
                    .collect::<String>()
            );
            let body = json!({"jsonrpc":"2.0","id":request["id"],"result":result}).to_string();
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",body.len(),body).as_bytes()).await.unwrap();
        }
    });
    let venue = UniswapV3::new(cfg).unwrap();
    let snapshot = PoolSnapshot {
        block: 42,
        block_hash: "fixed".into(),
        time_ms: 1_000_000,
        price: 2500.0,
        tick: -198000,
        tick_spacing: 1,
        liquidity: "1000000000000".into(),
        sqrt_price_x96: "1".into(),
        base_is_token0: true,
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let ids = venue.token_ids_at(owner, "0x2a").await.unwrap();
        assert_eq!(ids, vec!["7"]);
        let observations = venue
            .position_observations(
                &owner.to_string(),
                &[("core".into(), "7".into())],
                &snapshot,
            )
            .await
            .unwrap();
        let (position, revision) = &observations[0];
        assert_eq!(revision.split(':').count(), 7);
        assert!(revision.ends_with(":2:3"));
        assert!((position.unclaimed_quote - 0.001002).abs() < 1e-10);
        assert!((position.unclaimed_base - 0.000001).abs() < 1e-15);
        server.await.unwrap();
    })
    .await
    .unwrap();
}
