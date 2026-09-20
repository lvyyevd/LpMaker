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
# 中文状态摘要每分钟打印；数据刷新独立进行。
hyperliquid_interval_seconds = 60
robinhood_interval_seconds = 60
hyperliquid_refresh_seconds = 30
robinhood_refresh_seconds = 15
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

连接/写入分别有超时；EOF、Close、无响应、订阅拒绝、异常 JSON 都会触发重连。等待时间从 1 秒指数退避到 30 秒，稳定连接后恢复初始退避。重连后自动重订阅，使进程内共享观察失效，并重新核对账户和区块；不再补拉历史池日志。连续 60 秒没有收到帧触发心跳故障；连续 300 秒没有业务数据也会重连，Pong 和订阅 ACK 不会延长业务数据计时。

有界消息队列过载会触发恢复，不会无限占用内存。停止通知会打断握手和重试等待，监控退出时取消并回收 WebSocket 与刷新任务。策略结束也会停止监控和 nonce 后台任务。运行中 Ctrl-C 保留真实仓位；已经准备/发送但尚未确认的交易仍保留 pending，不假定取消或成功。

## 复用 WSS 与减少 RPC

已停用近 5 分钟成交量统计、历史区块定位和日志补数；旧成交量配置字段仅为兼容而保留，不能重新启用这些任务。`archive_rpc_url` 仍可供独立历史查询工具使用，实时监控不会为成交量访问它。

现有 EVM WebSocket 同时订阅区块头、池子日志和 Position Manager 的 NFT 事件，不另建监听连接。新鲜且连续的区块头经过 RPC 锚定后可替代重复查块；Swap 推送用于显示实时池价，明确标注未确认。余额、LP 本金和手续费继续以确认块上的真实读取为准，不能由 Swap 推算个人收益。

策略与监控复用同一链、池和数据源的短期观察；普通 Hold 决策直接使用本轮已核对库存。NFT 清单至少每分钟重新枚举，断线、缺块、重组、我方 NFT 变更及本地交易均使相关缓存失效。原观察时间不会因复用而刷新；交易前报价、余额和 nonce 继续强制核对。

状态摘要每 60 秒打印，HL 账户每 30 秒刷新，LP 每 15 秒检查，并显示该端点本报告周期实际发出的 RPC 请求数。遇到限流继续统一退避。细节与本机请求计数测试见 [RPC 请求优化](rpc-observations.md) 和 [RPC 限流恢复](rpc-rate-limits.md)。

## 状态与收益口径

- Hyperliquid 每 60 秒中文摘要：订阅币种的价格、数据时间、连接状态、实际账户余额/持仓、模拟空单（如适用）。缺少账户地址与真实零持仓分开表示。
- Robinhood 每 60 秒中文摘要：确认区块价格、每层上下界、区间内外、区间内相对位置、区间本金市值、策略状态、收益口径、WSS 实时参考价和 RPC 请求数量。
- Paper：模拟净值和损益、模拟换币成本/对冲费/资金费；LP 手续费和 gas 没有模拟，因此显示未知而不是编造收益。
- Live：待领取费用按实际链上 fee growth 计算；组合显示相对首次观察基准的账面变化。该变化含待领费用，但不扣链上 gas、也未扣除外部入出金影响，不能直接当作完整净利润。
- 只读监控没有策略历史基准时，收益显示不可用。历史缓存、账户快照都带时间，失败时保留之前的时间，不把旧数据伪装成新数据。

## 持仓时长与近 1 小时手续费 APR

每分钟的中文 LP 摘要显示每个 NFT 的持仓时长、近 1 小时手续费 APR、窗口内新增手续费、时间加权平均本金及实际观察时长；汇总行只统计当前仍持有的 LP。打印和刷新间隔保持原设置。

- 持仓时间保存在状态目录的 `lp_performance.json`，重启沿用。升级时按钱包和 Position Manager 匹配 `events.jsonl` / `event_archive` 中成功 mint 的回执；读取最多 64 MiB 的日志，不会为找旧时间无限扫描。无法恢复时显示“从首次观察起算，原建仓时间未知”，不会把程序启动时间伪装成实际建仓时间。日志中的“至少”以本地确认／观察时间为起点。
- APR 先对 WETH 和 USDG 的待领取数量分别求增量，再按最新池价把新增 WETH 手续费折成 USDG。旧 WETH 手续费的价格变化不计为新增手续费。NFT、流动性、手续费余额与价格在同一已确认区块读取，并检查区块是否发生变化。手续费计算依据 [Uniswap V3 Position](https://github.com/Uniswap/v3-core/blob/main/contracts/libraries/Position.sol) 的 fee-growth 口径。
- 本金不含待领手续费，采用快照之间的梯形积分计算时间加权平均值。最近 1 小时的边界落在两个样本之间时，对该段手续费和本金插值，因此这是采样估算。公式为：`手续费 APR = 窗口新增手续费 / 时间加权平均 LP 本金 × (365 × 24 × 3600 / 实际观察秒数) × 100%`。各层合计使用所有当前 NFT 都覆盖的共同窗口，按本金与时间汇总，不直接平均百分比。
- 不足 60 秒显示“待积累样本”；60 秒至 1 小时显示实际观察时长、“不足1小时，仅供参考”；历史覆盖满 1 小时才标为完整窗口。升级前只有累计待领费用、没有逐次采样的数据，不能补造为过去 1 小时的手续费。
- 增减流动性、领取手续费等操作使 NFT 的会计状态发生变化时，重新建立该 NFT 的手续费采样基准；历史数量下降、区块变化或采样间隔超过 `strategy.max_data_age_seconds`（默认 60 秒）也重新开始。持仓起点保留，但不跨越无法核对的区间推算 APR。新 NFT 单独计时；已退出的 NFT 从当前持仓统计移除。短暂停机可延续采样，长时间停机后先积累新窗口。
- 文件原子保存，样本只保留最近 1 小时及一个边界样本，避免长期运行无限增长。`run` 写入该文件；独立只读 `monitor` 可加载历史并在内存中继续采样，不修改策略状态。缓存损坏时明确提示并重新积累，不能把未知收益显示为零。

这是 **LP 手续费的单利年化 APR**，不包含币价损益、无常损失、Gas、兑换成本、合约手续费和资金费，不是组合净收益，也不是复利 APY 或未来收益承诺。Paper 模式没有模拟真实 LP 手续费，因此 APR 保持“未模拟”。

## EVM nonce 与交易恢复

`evm_nonce.json` 绑定 chain ID 和执行钱包，记录 latest、pending、不可回退的已使用 nonce 下界、当前交易哈希和准备时间。整个发送路径串行化；每次签名前成功获取链上 nonce 状态，不只依赖本地计数器。

实盘运行后台每 30 秒刷新；等待回执期间也定期更新。签名后、广播前先持久化 pending 和交易哈希，再记录 nonce 占用。广播只发送一次。响应丢失、超时、外部 pending 交易、nonce 回退、本地未决交易都阻止分配新的交易。超过 120 秒仍未解决会记录告警，不会自动替换原交易。

确认后核对交易哈希、canonical block hash 和确认数，先保存 NFT 结果，再在 nonce 管理锁内清理未决状态。回滚交易同样消耗 nonce。重启可从 pending 文件恢复 nonce 状态，即使上次崩溃发生在两个状态文件的写入间隙。不同钱包不能复用已有执行状态目录。

实盘启动先自动核对未决交易，也可用 `reconcile` 单独处理；它不会盲目换 nonce 重发。持仓、订单台账和检查点恢复见 [状态持久化与启动对账](recovery.md)。仅变更连接参数可继续使用现有策略状态；资金/策略/账户变更仍被配置指纹拦截。

## 日志

周期状态在控制台显示多行中文摘要；`run` 的完整 JSON 报告仍保存到状态目录的 `monitor_hyperliquid.json` / `monitor_robinhood.json`，并在 `debug` 级别输出。打印周期与刷新周期不属于交易配置指纹，升级时沿用现有状态目录，不必清空持仓或旧状态。

策略主动忽略的允许偏差内对冲余量（`within_hedge_deadband`）使用 `DEBUG`，不再每轮报 `WARN`；仍保存 `hedge_residual.json` 和业务事件，并在每分钟 Hyperliquid 中文摘要中显示上次核对的目标、实际空单、差额方向、金额及观察时间。该金额是敞口差额，不是亏损。最小下单限制、部分成交或执行后仍有余量等情况继续使用 `WARN`，交易失败及未决状态的告警保持原样。日志分级不改变对冲阈值或下单行为。

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
