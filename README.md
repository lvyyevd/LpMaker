# LpMaker

Rust 浓缩流动性管理与永续合约对冲项目。当前接入 **Robinhood Chain 的 Uniswap V3 WETH/USDG 0.01% 池**，使用 **Hyperliquid ETH 永续**对冲。Hyperliquid 连接器也支持动态发现和交易其他原生永续代币。

默认 `paper`。本项目创建和验证期间没有读取真实私钥、下单、授权代币或发送链上交易。

账户授权验证、只读故障恢复、小额补单、日志容量管理及健康检查见 [运行可靠性与验收](docs/production.md)。回归测试仍不等于已完成长期实盘验收。

## 快速运行

```bash
cd /Users/wangzheng/lvyyevd/code/rustcode/LpMaker
cargo build --locked
cargo run --locked -- check
cargo run --locked -- pool snapshot
cargo run --locked -- watch --coins ETH,BTC,SOL --seconds 30
cargo run --locked -- run --once
```

持续模拟策略：

```bash
cargo run --locked -- run
```

`run` 自动启动 Hyperliquid 官方 WebSocket 与 Robinhood PublicNode WebSocket 监听，并按 `poll_seconds` 周期使用可核对的 RPC/HTTP 快照决策。独立 `monitor` 是只读监控，不加载私钥、不下单，可与策略同时运行；`pool watch` 同样进入这套监控。`watch` 保留 Hyperliquid 原始 JSONL 流，同时每 30 秒输出价格、账户持仓摘要。

```bash
# 只读监听两端：默认每 30 秒输出 Hyperliquid，每 15 秒输出 LP 状态
cargo run --locked -- --config config/local.toml monitor
# 有时限的只读连通检查
cargo run --locked -- --config config/local.toml monitor --seconds 90
# 独立 200 美元模拟配置；不需要私钥
cargo run --locked -- --config config/paper-200.toml run
```

`hyperliquid.account` 填实际账户地址即可订阅真实持仓；未填写时输出 `account_not_configured`，不会把未知持仓当成零。Paper 模式另外标记模拟空单。实盘 LP 的只读查看可填写 `liquidity.owner`（公开地址）；运行实盘后也会从持久化的执行钱包身份读取，不会为监控提取私钥。

连接、nonce、日志配置和验证结果见 [监听与生命周期](docs/lifecycle.md)。持仓、限价单、策略阶段的保存文件及启动对账顺序见 [状态持久化与启动对账](docs/recovery.md)。

初次启动会加载约 182 根小时 K 线。行情若符合下跌条件，保持 `Paused`、稳定币等待，是预期行为。Ctrl-C 停止进程，**不会自动平掉现有实盘仓位**。

## 已实现

| 模块 | 功能 |
|---|---|
| Hyperliquid 行情 | 全市场中间价、L2 深度、成交、小时 K 线、资产上下文；按配置订阅多个代币 |
| Hyperliquid 账户 | 持仓、保证金、挂单、订单状态、成交历史、资金费率；账户 WebSocket 事件 |
| Hyperliquid 交易 | EIP-712 / MessagePack 签名；ALO Maker、GTC、IOC；Reduce Only；止盈止损触发单；按 OID/Cloid 撤单、改单、撤全部、定时撤单；杠杆与逐仓保证金调整 |
| Robinhood V3 | 链 ID / 代币 / 精度 / factory / router 校验；固定区块快照；Swap/Mint/Burn/Collect 监听；NFT 持仓与未领取手续费读取 |
| LP 执行 | 精确额度授权、mint、increase、decrease+collect+burn、手续费领取、同池 WETH/USDG 兑换；ABI 编码、eth_call 模拟、gas 上限、滑点和 deadline、签名广播与确认 |
| 策略 | 宽窄两层独立区间；突破确认；按 ETH 实际数量对冲；快跌、波动突增、慢速单边跌、价差、过期数据限制；冷却、连续健康小时、25% 分批恢复；总净值回撤停机 |
| 状态 | 独占执行锁、原子状态文件、操作日志、持久化 nonce/Cloid/交易哈希、NFT 注册表、未完成操作查询与恢复 |
| 模拟 | 实时 paper；小时 K 线回放；Maker 必须在后续采样价格穿越才模拟成交；费用与方向损益分开记录 |

Hyperliquid 当前覆盖默认 `meta.universe` 的原生永续，资产 ID、数量精度和最大杠杆动态读取。**HIP-3 自定义 DEX、现货交易、充值、提现和跨链桥不在本版接口范围内。** 3× 是保证金倍数，持有 2 ETH 的完整对冲目标仍是做空 2 ETH。

## 策略默认参数

所有阈值是待验证的研究初值，并非最优参数。

- 总预算 10,000：LP 6,000、Hyperliquid 保证金 2,500、外部预留 1,500。
- 核心层：LP 预算的 2/3，几何区间 `[P/1.08, P×1.08]`。
- 窄区间层：LP 预算的 1/3，区间 `[P/1.02, P×1.02]`。
- 区间内默认不做方向对冲；下破的那一层按实际 ETH 数量对冲，回到下界上方 0.4% 后解除该层保护。
- 正常调仓优先 ALO；等待 15 秒后撤单并核对实际持仓。保护性调仓最多等待 2 秒，未完成部分可用带价格上限的 IOC。IOC 也可能部分成交，不能当成保证成交。
- 区间外额外突破 1%，连续 8 次 15 分钟边界观察后，且风险条件允许，才重设相应层。**没有“出区间满 4 小时强制重设”。** 观察时点为边界后的首次轮询，并非交易所精确 15 分钟收盘；漏掉观察会清零连续计数。
- 最新完整小时跌幅或现价相对最近完整小时收盘下跌超过 1.5%：暂停。
- 最近 6 个完整小时收益率标准差 / 此前 168 小时标准差 > 1.8：暂停。分母下限 1e-6，避免接近零导致除零。
- 完整小时价格低于 EMA20、EMA20 < EMA60，且两条均线下行：暂停，即使波动率很低。
- 池价格与永续价格偏差 > 100 bps：暂停。此限制也能拦截部分稳定币偏离，不能替代独立稳定币 USD 价格源。
- 暂停后撤出 LP、将 ETH 换成 USDG，并根据实际余额同步缩减空单；保留稳定币等待。
- 至少冷却 6 小时，连续 6 个新完成小时满足：波动比 < 1.2、无下跌信号、没有新低、价格不低于快均线。恢复 25% LP；持续健康每 4 小时增加 25%。已有区间通过 increase 增加资金。
- 组合净值相对高水位回撤 5%：进入 `Halted`，退出 ETH/LP，**不会自动恢复**。需检查原因、核对余额，再显式维护策略状态和净值基准。

暂停规则优先于重设规则。过期或无效价格禁止依赖该价格发送交易；有效行情恢复后再处理敞口。LP 重设过程中再次检查波动与趋势，如果行情恶化则中止新增，保留可核对的恢复状态。

## 常用命令

```bash
cargo run -- info assets
cargo run -- info book ETH
cargo run -- info funding ETH --start-ms 1788771600000
cargo run -- info candles ETH --interval 1h --start-ms 1788771600000 > /tmp/eth-hours.json
cargo run -- replay --candles /tmp/eth-hours.json --output /tmp/lpmaker-replay.json
cargo run -- pool watch --seconds 30
cargo run -- status
```

查询账户前在配置中填写 `hyperliquid.account`，应填写账户所有者地址，**不是 API agent 的地址**。

```bash
cargo run -- --config config/local.toml info account
cargo run -- --config config/local.toml info orders
cargo run -- --config config/local.toml info order-status 0x00000000000000000000000000000001
```

完整参数见 `cargo run -- --help`、`trade --help`、`lp --help`。

## 实盘配置与边界

1. 复制配置为被 git 忽略的 `config/local.toml`，改为 `mode = "live"`、独立 `state_dir = "data/live"`；填写账户所有者地址。
2. 使用专用 EVM 钱包与专用 Hyperliquid 账户 / 子账户。EVM 钱包中的配置代币余额属于本策略库存，不能混入其他用途的 ETH/USDG。外部 1,500 预留资金不会被程序自动读取或调入保证金，净值中按配置常数计入；gas 余额单独准备。
3. 自行通过本机秘密管理方式设置 `LPMAKER_EVM_PRIVATE_KEY`、`LPMAKER_HL_PRIVATE_KEY`。不要将私钥写入 TOML、代码、命令历史或日志。程序不会自动加载项目 `.env`；如使用该本机私钥文件，需先 `source .env`。
4. Hyperliquid 可以使用已授权的 API agent 签名密钥；资金、授权和平台使用资格由账户持有人管理。初始 LP 钱包可持有 USDG/WETH；当前实现使用 WETH，不自动 wrap 原生 ETH。
5. 已存在 LP 需要先 `lp --execute import <layer> <token_id>`。启动会对照链上 NFT 清单，遇到未登记或丢失的同池有效 NFT 会停机。

真实动作必须同时满足 live 配置和 `--execute`：

```bash
# 以下命令会真实交易；仅在已完成账户和小额联调后运行。
cargo run -- --config config/local.toml run --execute
cargo run -- --config config/local.toml trade --execute leverage ETH --leverage 3
cargo run -- --config config/local.toml trade --execute limit ETH --price 2500 --size 0.01 --tif Alo
cargo run -- --config config/local.toml trade --execute close ETH
cargo run -- --config config/local.toml lp --execute collect satellite
```

跨链 LP 与 Hyperliquid 不能原子成交。程序在 LP 调整期间先尽量对冲库存，再依据链上确认后的余额调整空单；卖出 ETH 到平空之间仍有短暂敞口。交易超时、未知结果、部分成交或恢复信息不一致会留下日志并停止新增交易，不能用重启来假设已经成功。

EVM nonce 按钱包和链持久化，每 30 秒刷新 `latest/pending`；每次发送前重新核对，广播、确认、回滚及重启恢复有独立记录。结果未知时不自动换 nonce 或提高 gas 重发。`pending.json` 存在时，实盘启动先自动对账，也可单独运行 `reconcile`。EVM 按原交易哈希核对回执与确认区块，Hyperliquid 按原 Cloid 查订单；不会更换 nonce 盲目重发。更新 RPC、WebSocket、nonce 刷新周期不会重置已有仓位；更改资金、策略、模式或 Hyperliquid 账户仍需要明确的状态迁移。重启时会核对订单台账：确认属于策略的遗留对冲挂单撤销后重新计算目标；手工或未知来源订单会阻止启动。无法可靠证明结果的保证金变更等操作会保留 pending，必须人工核对；不要直接删除文件继续运行。未完成的 LP 工作流在对账完成后的下次实盘启动会转为退出到稳定币，再进入冷却期。

这是一版可编译、可模拟、带真实签名/执行接口的实现。**实盘写路径没有使用真实资金验证，也未完成第三方审计或完整长周期回测。** 不应把单元测试或公共行情连通性等同于已验证的实盘盈利能力。

## 模拟结果如何理解

- 不填造 LP 手续费收入，不根据池年化倒推收益。
- 实时 paper 资金费使用公开历史费率和采样时仓位/价格近似；没有完整历史 oracle / 逐笔仓位时，不能当成准确账单。
- 小时 replay 是逻辑检验工具：无逐笔顺序、无订单队列、spot/perp 同价、无历史资金费、无强平模型；不适合直接挑最优区间或估计利润。小时采样无法满足连续的 15 分钟突破观察，因此此回放不会凭空补齐缺失观察来触发正常重设。
- Paper/replay 不模拟真实 LP 工作流的多次临时对冲及 gas；收取配置的换币费用/滑点和常规对冲费用。实盘成本可能更高。
- 实盘 LP 余额含截至确认区块的待领手续费，HL 净值来自账户接口。USDG、USDC 均以 1 美元作账，外部预留为常数；不是完整跨链会计系统。

## 扩展结构

```text
src/domain.rs           平台无关的行情、LP、仓位、决策及能力接口
src/strategy/           纯策略与指标；不访问网络、不签名
src/hyperliquid/        信息、WebSocket、订单精度、签名、合约操作
src/evm/               通用 EVM RPC、V3 ABI、事件与交易执行
src/engine.rs           paper / live 编排与恢复
src/store.rs            原子状态、互斥执行、日志、未完成操作
config/robinhood.toml   当前池和默认策略配置
```

同协议的新 Ethereum/L2 部署可复用 V3 适配器，配置链 ID、RPC、池、工厂、NFPM、SwapRouter02、代币精度及对应对冲币。启动时仍须验证链上实际部署。非 V3 平台实现 `LiquidityVenue` 与 `LiquidityExecutor` 并在启动装配处注册；无需把协议 ABI 或 NFT 概念引入策略。V4、其他 AMM 的仓位/费用模型不同，不能仅改地址套用 V3。

详见 [架构与恢复](docs/architecture.md)、[验证记录](docs/verification.md)、[协议来源](docs/sources.md)。

## 验证

```bash
cargo fmt --all -- --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```
