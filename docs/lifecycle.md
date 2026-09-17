# 监听、nonce 与日志生命周期

## 配置与启动

`config/robinhood.toml` 和本机 `config/local.toml` 已写入：

```toml
[hyperliquid]
ws_url = "wss://api.hyperliquid.xyz/ws"

[liquidity]
rpc_url = "https://robinhood-rpc.publicnode.com"
ws_url = "wss://robinhood-rpc.publicnode.com"
archive_rpc_url = "https://rpc.mainnet.chain.robinhood.com"
nonce_refresh_seconds = 30
pending_warn_seconds = 120
# owner = "0x实际LP钱包公开地址"

[websocket]
heartbeat_seconds = 20
heartbeat_timeout_seconds = 60
idle_timeout_seconds = 300
connect_timeout_seconds = 15
write_timeout_seconds = 10
reconnect_initial_ms = 1000
reconnect_max_seconds = 30

[monitoring]
hyperliquid_interval_seconds = 30
robinhood_interval_seconds = 15
volume_window_seconds = 300
backfill_blocks = 6000
refresh_timeout_seconds = 45

[logging]
level = "info"
directory = "data/logs"
retained_files = 14
```

以上是相关字段的说明，不是可独立启动的完整 TOML。用项目完整配置运行：

```bash
cargo run --locked -- --config config/local.toml check
cargo run --locked -- --config config/local.toml monitor --seconds 90
cargo run --locked -- --config config/paper-200.toml run
```

`monitor`、`pool watch`、`watch` 不签名、不下单，无需加载 `.env`。`run` 中自动带两端监控。`config/paper-200.toml` 默认总预算 200、LP 120、模拟对冲保证金 60、备用 20，对冲差额阈值 12 美元。这是模拟预设，不是实盘已验证的参数。

已有本地配置的账户、资金参数、mode、state_dir 没有被本次节点改造覆盖。真实 LP 的只读监控需要 `liquidity.owner` 或该状态目录中已经持久化的执行钱包身份。Paper 模式输出模拟 LP；Hyperliquid 真实账户与模拟空单分别显示。

## 连接与恢复

两端连接共享同一生命周期实现，支持 `ws://`、`wss://`。Hyperliquid 使用原生订阅协议和应用层 `{"method":"ping"}`；EVM 使用 `eth_subscribe(newHeads/logs)` 和 WebSocket Ping/Pong。EVM 普通 RPC 也可配置为 HTTP、HTTPS、WS、WSS。

连接/写入分别有超时；EOF、Close、无响应、订阅拒绝、异常 JSON 都会触发重连。等待时间从 1 秒指数退避到 30 秒，稳定连接后恢复初始退避。重连后自动重订阅，重新核对账户快照，历史池日志独立补齐。连续 60 秒没有收到帧触发心跳故障；连续 300 秒没有业务数据也会重连，Pong 和订阅 ACK 不会延长业务数据计时。

有界消息队列过载会触发恢复，不会无限占用内存。停止通知会打断握手和重试等待，监控退出时取消并回收 WebSocket 与刷新任务。策略结束也会停止监控和 nonce 后台任务。运行中 Ctrl-C 保留真实仓位；已经准备/发送但尚未确认的交易仍保留 pending，不假定取消或成功。

## PublicNode 历史查询限制

本地实测 PublicNode 免费 HTTP 对部分历史 `eth_getLogs` 返回 HTTP 403：`Archive requests require a personal token`。主 RPC、发交易以及 WS 订阅仍使用 PublicNode；仅历史日志和历史区块头走 `archive_rpc_url`。默认该字段使用官方公开 Robinhood RPC，也可在本机换成具备历史权限的 PublicNode URL。两节点会核对 chain ID 与相同高度的 canonical block hash。不会把交易发到历史节点。

5 分钟成交量通过区块时间二分定位起点，每次只汇总确认区块中的 Swap，按 USDG 一侧绝对金额计一次，另外报告 WETH 数量。部分节点的日志 `blockTimestamp` 为 `0x0`，所以不依赖这个可选字段。历史请求按 200 个区块分批、最多 4 路并行；按 `(blockHash, logIndex)` 去重，发现游标区块重组则重新构建窗口。

历史补数与 LP 快照刷新独立运行。输出包含 `complete`、`as_of_ms`、`volume_age_ms`、错误和刷新状态：首次补数或慢节点期间不把不完整数据当成完整的 5 分钟成交量。打印周期不代表所有上游数据都恰好同时更新。

## 状态与收益口径

- Hyperliquid 每 30 秒：订阅币种的价格、数据时间、连接状态、实际账户余额/持仓、模拟空单（如适用）。缺少账户地址与真实零持仓分开表示。
- Robinhood 每 15 秒：确认区块价格、每层上下界、区间内外、区间内相对位置、距两侧百分比、区间本金市值、近期确认交易量、策略状态及收益口径。
- Paper：模拟净值和损益、模拟换币成本/对冲费/资金费；LP 手续费和 gas 没有模拟，因此显示未知而不是编造收益。
- Live：待领取费用按实际链上 fee growth 计算；组合显示相对首次观察基准的账面变化。该变化含待领费用，但不扣链上 gas、也未扣除外部入出金影响，不能直接当作完整净利润。
- 只读监控没有策略历史基准时，收益显示不可用。历史缓存、账户快照都带时间，失败时保留之前的时间，不把旧数据伪装成新数据。

## EVM nonce 与交易恢复

`evm_nonce.json` 绑定 chain ID 和执行钱包，记录 latest、pending、不可回退的已使用 nonce 下界、当前交易哈希和准备时间。整个发送路径串行化；每次签名前成功获取链上 nonce 状态，不只依赖本地计数器。

实盘运行后台每 30 秒刷新；等待回执期间也定期更新。签名后、广播前先持久化 pending 和交易哈希，再记录 nonce 占用。广播只发送一次。响应丢失、超时、外部 pending 交易、nonce 回退、本地未决交易都阻止分配新的交易。超过 120 秒仍未解决会记录告警，不会自动替换原交易。

确认后核对交易哈希、canonical block hash 和确认数，先保存 NFT 结果，再在 nonce 管理锁内清理未决状态。回滚交易同样消耗 nonce。重启可从 pending 文件恢复 nonce 状态，即使上次崩溃发生在两个状态文件的写入间隙。不同钱包不能复用已有执行状态目录。

实盘启动先自动核对未决交易，也可用 `reconcile` 单独处理；它不会盲目换 nonce 重发。持仓、订单台账和检查点恢复见 [状态持久化与启动对账](recovery.md)。仅变更连接参数可继续使用现有策略状态；资金/策略/账户变更仍被配置指纹拦截。

## 日志

控制台输出和 `data/logs/lpmaker.<日期>.jsonl` 同时写入，按天轮转、保留 14 个匹配文件。级别由 `[logging].level` 控制，不受启动环境中通用 `RUST_LOG=warn` 遮挡；可改为 `debug` 查看每个 RPC 的方法、request ID、耗时及重试次数。

业务操作另写状态目录的 `events.jsonl`：决策、模拟执行、交易准备、nonce 占用/刷新/确认、广播、回执与错误。LP 操作包含层、金额、区间/tick、原始代币额度、滑点或 token ID；Hyperliquid 包含方向、价格、数量、TIF 与订单 ID。日志不打印私钥、签名请求体或原始已签名交易；恢复所需原始交易只保存在已有的 pending 状态文件中。

## 验证范围

验证使用公开主网数据、隔离的 200 美元 paper 状态及本地 WebSocket/RPC 模拟服务，不加载用户 `.env`。本地签名用公开固定测试向量，仅向回环地址的模拟服务广播。

故障测试覆盖重连重订阅、握手期间退出、Pong 持续但业务数据超时、Ethereum 订阅 ID 映射、WS RPC、并发 nonce、响应丢失、回滚、崩溃间隙恢复、换钱包拒绝、配置迁移限制、重复日志、区块重组和零日志时间戳。生产 300 秒业务空闲参数有断言，超时测试使用缩短到 2 秒的同一实现。

真实下单、授权、LP mint/burn/swap 尚未以用户真实资金执行。公共连通、模拟运行和本地签名测试不能替代实盘小额交易核对。

官方参考：
- [Hyperliquid WebSocket 心跳](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/websocket/timeouts-and-heartbeats)
- [Hyperliquid 订阅协议](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/websocket/subscriptions)
- [PublicNode Robinhood 节点](https://robinhood.publicnode.com/)
