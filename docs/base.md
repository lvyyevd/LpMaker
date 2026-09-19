# Base WETH/USDC 接入与原策略升级

支持池：`0x6c561B446416E1A00E8E93E221854d6eA4171372`，Base 主网（8453），Uniswap V3，WETH/USDC，费率 3000 / 1,000,000 = 0.3%，tickSpacing = 60。这不是 0.01% 费率池。

## 运行入口

`config/base.toml` 默认 paper，预算和 Robinhood 的 200 美元配置一致：LP 120、对冲保证金 60、备用 20；主/辅助层权重 2/3 和 1/3，宽度 0.08 和 0.02。仍使用同一个策略状态机、Hyperliquid ETH 永续、3 倍逐仓、Maker 优先、紧急 IOC、快跌/波动/趋势暂停与分阶段恢复。

```bash
cargo build --release --locked
./target/release/lp-maker --config config/base.toml check
./target/release/lp-maker --config config/base.toml pool snapshot
./target/release/lp-maker --config config/base.toml monitor --seconds 90
# 模拟执行一轮，可写入独立的模拟状态目录，不发送真实交易
./target/release/lp-maker --config config/base.toml run --once
```

额外的链上只读验收包含合约身份、买卖双向 Quoter 报价和 Base 附加费用：

```bash
cargo run --locked --example base_readonly
```

默认 HTTP/WS 为 Base PublicNode；支持 `http://`、`https://`、`ws://`、`wss://` RPC，WS 订阅支持 `ws://`、`wss://`。可以分别修改主 RPC、历史 RPC 和 WS。沿用断线退避重连、5 分钟无业务消息重连、定期 nonce 对账。

## 实盘实例隔离

默认状态目录为 `data/base-weth-usdc-3000-paper`，日志目录为 `data/logs/base-weth-usdc-3000`，不读取 `data/live-200` 的原有 Robinhood 仓位。

准备实盘时复制为自己的 Base 配置，填写 `hyperliquid.account`（主账户地址）及可选的 `vault`（真实子账户地址），改为 `mode = "live"`，另设例如 `data/base-weth-usdc-3000-live`。私钥环境变量为 `LPMAKER_BASE_EVM_PRIVATE_KEY` 和 `LPMAKER_BASE_HL_PRIVATE_KEY`；不要把密钥写入 TOML 或提交到 Git。Base 链钱包需要 WETH/USDC 和用于 Gas 的原生 ETH；Hyperliquid 需要已经核对为可用于 ETH 做空的保证金。

**一份配置管理一个池，一份状态目录对应一个实例。** 如两条链同时实盘运行，必须使用各自独立的 Hyperliquid 主账户或子账户。两个 API 钱包若代理同一个交易账户，仍然会共享 ETH 空单、资金和订单，不能用于隔离。当前没有多池合并对冲账本，也没有跨主机账户锁；状态目录不同不代表交易账户隔离。同链多池也应使用独立 EVM 钱包，避免 nonce 和钱包库存互相影响。

## Base 的执行差异

- 地址由 `src/liquidity/chains/base/` 定义，池由 `pools/weth_usdc.rs` 定义。启动仍核对链 ID、合约代码、token0/1、精度、费率、Factory 注册、Position Manager、Router、Quoter 和 tick 间距。Router02 已实测使用 `factory()`，不能猜测为 `factoryV3()`。
- Base 的 60-tick 间距会使对齐后的配比偏离 50/50。建仓前按真实对齐区间计算 WETH 需求；mint/increase 根据扣除池费后的真实余额同比缩小代币额度。不会为凑额度额外重放换币。
- 换币通过 QuoterV2 在明确区块上 `eth_call`，将已经扣除池费和价格影响的输出再乘滑点系数。报价失败就停止，不回退成未扣池费的价格。这样不会把池子自身的 0.3% 费用误当成可容忍滑点的全部。
- Base Gas 预算加入 GasPriceOracle 的 L1 费用估计上界和 operator 费用，并加配置的费用预留；再检查总费用预算与 ETH 余额。Oracle 无法查询时停止发送。估计与预留仍不能保证任意突发涨费下成功。
- 中文状态显示 Base、USDC、WETH；结构化报告使用 `principal_value_quote`、`unclaimed_fees_quote`、`fees_quote`、`average_principal_quote`、`volume_quote`、`volume_base`，币种由 `market` 字段指明。Base 报告保存在 `monitor_liquidity.json`。

手续费档位不同会改变换币成本和 LP 收费来源；共用策略参数不代表两个池具有相同收益或同样适合这些宽度。APR 仍是窗口内 LP 手续费的单利年化，不是净收益。Paper 不模拟真实 LP 手续费。

## Robinhood 重启兼容

本次不修改 `config/local.toml`、Robinhood 的配置示例、经济参数或 `src/strategy/` 状态机。已知的资金账本范围、备用金口径和恢复期 Recenter 比例等旧行为也未在模块化过程中另行调整。

保留以下兼容边界：

- CLI 命令、默认配置、`mode` 和 `--execute` 要求不变。
- `checkpoint.json` 仍是 schema 1；`config.json` 的经济/账户配置指纹不变；不会因 Rust 文件移动而重建仓位或重置首次建仓标记。
- NFT、挂单、nonce、pending、workflow、持仓时间和 APR 历史仍使用原文件名与格式。未知交易继续保留原哈希/Cloid，先对账，禁止盲目重发。
- 原 `robinhood_interval_seconds` / `robinhood_refresh_seconds` 字段继续有效；新别名 `liquidity_interval_seconds` / `liquidity_refresh_seconds` 仅改变写法。
- Robinhood 的 `monitor_robinhood.json` 和原 USDG 数值字段仍保留，另外提供通用字段；旧 `evm::UniswapV3`、`evm::tx::Executor` 导入路径仍可用。
- Robinhood 的换币最低输出、等值建仓配比和 Gas 检查保持原逻辑，Base 的新增处理只作用于 chain ID 8453。

服务器更新代码后，用原先的“编译、正常停止旧进程、等待进程锁、按原配置启动”命令重启即可；保留完整 `data/live-200/`。不要把 Base 配置覆盖到 `config/local.toml`，不要删除旧状态，也不要在原状态目录切换链或池。原有未完成 workflow 在重启时仍按旧恢复规则处理，并不保证继续保留中断前的全部 LP。

## 核实来源

- [用户指定的 Uniswap 池](https://app.uniswap.org/explore/pools/base/0x6c561B446416E1A00E8E93E221854d6eA4171372)
- [Uniswap 官方 Base 部署](https://developers.uniswap.org/docs/protocols/v3/deployments/v3-base-deployments)
- [QuoterV2 ABI](https://github.com/Uniswap/v3-periphery/blob/main/contracts/interfaces/IQuoterV2.sol)
- [Router V3 实现](https://github.com/Uniswap/swap-router-contracts/blob/main/contracts/V3SwapRouter.sol)
- [OP Stack GasPriceOracle](https://github.com/ethereum-optimism/optimism/blob/develop/packages/contracts-bedrock/src/L2/GasPriceOracle.sol)

2026-09-18 使用公开 Base RPC 只读核对：Factory、Manager、Router、Quoter、WETH/USDC 精度、fee=3000、tickSpacing=60、池快照、双向 Quoter 和附加 Gas 查询均通过。没有读取真实私钥、授权代币或广播交易。模拟节点的签名/恢复测试使用固定公开测试密钥；这些检查不能替代长期实盘验收。

本地验证还包括：150 项自动测试通过；旧版本配置指纹与 schema-1 检查点兼容；Base 模拟节点执行换币、mint、重启恢复 NFT、decrease/collect/burn；L1 费用超预算阻止发送；广播响应丢失后核对原交易而不重复发送。70 秒公开 WS 监控两端连接正常，一分钟摘要及 300 秒成交量完整回补正常；独立 paper `run --once` 完成首次 core/satellite 建仓并保存 Established 状态，未触碰 Robinhood 状态目录。
