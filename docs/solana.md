# Solana / Meteora DLMM 独立适配

本模块接入 SOL/USDC 池 `5rCf1DM8LjKTw4YqhnoLcngyZYeNnQqztScTogYHAS6`，Hyperliquid 对冲币种为 SOL。

[30 天研究报告](../backtests/solana/2026-09-18/REPORT.md)包含数据覆盖、12 组参数、训练/独立测试窗口、租金约束及费用敏感性。当前没有证据证明个人 LP 手续费足以覆盖模型损耗；默认配置仅用于模拟。

## 模块边界

```text
src/bin/lp-maker-solana.rs     独立 CLI；原 lp-maker 的默认入口不变
src/solana/
  config.rs                   Solana 配置与经济身份，拒绝 EVM 状态目录
  dlmm.rs                     Solana 快照、bin 数学、研究模拟账本
  runner.rs                   独立运行/恢复流程，复用纯 Strategy
  journal.rs                  Solana 签名与 blockhash 生命周期
  bridge.rs                   与官方 SDK 子进程的窄 JSON 接口
  performance.rs              持仓时间/滚动费用 APR 的 Solana 身份适配
  backtest.rs                 数据验证、12 候选比较、费用敏感性
adapters/solana/
  pool.mjs                    池、program、mints、精度、genesis 白名单
  bridge.mjs                  Meteora 官方 SDK 的观测/建仓/增仓/退出/swap
  grpc.mjs                    Triton Yellowstone confirmed 账户订阅
  transaction.mjs             签名编码、payer 校验、增量租金计算
src/hyperliquid/hedge.rs       EVM/Solana 共用的原 maker → cancel → IOC 流程
scripts/solana/                公共历史下载与报告生成
```

Solana 不使用 EVM nonce、NFT ABI、sqrtPrice 流动性公式或旧 `checkpoint.json`。为了复用当前纯策略输入，策略帧只传价格/时间；其中 V3 专用元数据为空，不能用于链上执行。

官方 Meteora SDK 使用 Node.js；Rust 继续负责风险、对冲、状态持久化与运行控制。版本固定在 `adapters/solana/package-lock.json`。原 EVM 运行不依赖 Node。

## 配置与隔离

- 模板：`config/solana.toml`，默认 `paper`，状态 `data/solana-sol-usdc-paper`，日志 `data/logs/solana-sol-usdc`。
- token 仅存在本地 `.env.solana`（0600、Git 忽略），未写入模板、锁文件或研究数据。当前 RPC/gRPC 完整地址仍待用户提供；不要根据 token 猜测 endpoint。
- 环境变量 `LPMAKER_SOL_RPC_URL`：服务商完整 HTTP(S) RPC URL，若服务商把认证放在路径中，应保存整个 URL。
- `LPMAKER_SOL_GRPC_URL`：完整 HTTP(S) gRPC 服务地址；`LPMAKER_SOL_GRPC_TOKEN` 通过 `x-token` 使用。
- `LPMAKER_SOL_PRIVATE_KEY`：仅实盘读取的 Solana 钱包 secret key（64 字节数组或 base58 secret key）。与 EVM 私钥不同。
- `LPMAKER_SOL_HL_PRIVATE_KEY`：该实例专用的 Hyperliquid API wallet key；`hyperliquid.account` 填其主账户，或配置专用子账户 `vault`。
- 实盘还需 `solana.owner` 公钥、单独 live 状态目录、`mode="live"` 和 `run --execute` 双重开关。不要让新实盘配置复用 paper 状态。
- 同时运行 Robinhood/Base/Solana 时，使用不同的 Hyperliquid 主账户或子账户。多个 API wallet 指向同一账户不会隔离仓位。本程序发现同账户其他币种持仓或未托管订单会阻止启动；跨主机并发仍需部署侧隔离。
- 使用专用 Solana LP 钱包。LP 资金放在 USDC/WSOL；原生 SOL 单独支付租金与 Gas，不会被 swap 清空。`reserve` 是固定美元记账预算，组合净值不代表已经扣掉实际 Gas 和出入金的净利润。

## 策略

当前 200 美元候选使用 120 美元 LP、60 美元 HL 保证金、20 美元备用预算。主层 2/3、约 ±2.5%；辅层 1/3、约 ±0.8%。几何边界对齐到 4 bps 的 bin。实盘采用官方 SDK Spot 分配，建仓前按真实 active bin 配比获取所需 SOL，而不是直接假设 50/50。

区间内，对 LP SOL 库存对冲 50%；下破本层下沿时，该层升为全额对冲，重返下沿上方 0.4% 后解除保护。钱包 WSOL 余量和跨平台操作中的临时库存另行对冲。3 倍逐仓用于保证金比例，不放大目标 SOL 数量。

正常对冲挂 post-only 单，等待 15 秒后撤单并核对实际成交；紧急时最多等待 2 秒，剩余量允许 IOC。成交未确认时保持持久化阻塞。交易所最小订单和数量精度可能留下小额余量，代码不会为了满足最低限额强行放大风险。

快跌 1.5%、短长波动率比 1.8、持续下降趋势、跨场价格偏离超过 1% 均沿用原纯策略的暂停逻辑。首次建仓免冷却，但不免市场检查。之后仍需 6 小时冷却与 6 个健康小时，25% 分批恢复；恢复期重新居中不会恢复为 100% 仓位。5% 组合回撤进入 Halted，不自动解除。

初始换币前检查整个 LP 的最低仓位租金和保证金。签名前逐笔模拟实际新增/扩容账户租金、检查交易费上限和原生 SOL 余量。当前 125-bin + 41-bin 两仓位的 position 租金观察约 0.1150 SOL，另需至少 0.03 SOL Gas 和 ATA/bin-array 余量。尚未创建的 bin array 会增加占用，最终以模拟为准。

## 重启与未知交易

`solana_checkpoint.json` 保存策略阶段与 paper 账本；`solana_positions.json` 保存层名到 position 公钥和首次确认时间。启动严格核对全部该钱包在池中的 position，发现缺失或未追踪仓位就停止，不盲目重新建仓。

每笔链上交易按顺序执行：构建 SDK 指令 → 新 blockhash → 本地签名 → RPC 模拟与费用/租金检查 → `solana_pending.json` 原子落盘并 fsync → 广播 → 按原 signature 核对 confirmed/finalized → 更新 position 映射 → 清除 pending。RPC 报错、丢回复、确认超时均保留原签名，后续操作不能另签一笔重复发送。

blockhash 过期但历史查询仍没有明确结果时也保持阻塞，**不会自动清掉 pending**。可用 `reconcile` 查询原签名；如果节点历史不足，需换同链完整历史 RPC 核对，不能通过删状态解决。待链上操作明确后，未完成的 LP 工作流会在启动时退出已核实库存、保留冷却，然后重新评估。

Hyperliquid 原有订单 journal、cloid、撤单后的成交核对与未决操作恢复共用。原 Robinhood/Base 的检查点 schema、文件名、配置经济指纹和风险阈值未为 Solana 改写。

## 监听与日志

Triton 使用 Yellowstone gRPC，订阅目标池 confirmed 账户变化，20 秒心跳，60 秒无消息或 5 分钟无池数据重连，退避 1～30 秒；pong 不计为池数据。连接恢复后以 confirmed RPC 全量核对，增量事件不直接触发资金操作。RPC 请求有超时；子进程超时关闭，写操作依靠原签名恢复。

Hyperliquid 使用原官方原生 WS，继承已有连接重试和心跳。Solana 每 15 秒观察、每 60 秒中文摘要，显示区间、仓位/对冲、阶段、费用与持仓时间。实际操作记录在轮转日志与事件 journal。SDK 外部异常文本不直接输出，避免包含带凭据的 RPC URL。

实盘 1 小时 APR 按费用 **token 数量差分**计算，再用当前价估值，分母为时间加权 LP 本金；不会把 SOL 涨价造成的旧费用升值算为新收费。历史样本独立存储在 `solana_performance.json`；加减仓、领取、计数下降、链回退或长时间无数据会重新积累可比样本，持仓起始时间保留。RPC SDK 的多个 confirmed 读取不是同一个原子 slot 快照，因此该指标是监控估计。在线 paper 不假造个人 LP 费，也未计资金费；离线回测单独计历史 funding。

## 运行

从项目根目录执行。配置检查和离线回测不需要 RPC、gRPC 或私钥：

```bash
npm ci --prefix adapters/solana --ignore-scripts
cargo build --release --locked
./target/release/lp-maker-solana --config config/solana.toml check
./target/release/lp-maker-solana --config config/solana.toml backtest \
  --data backtests/solana/2026-09-18 \
  --output backtests/solana/2026-09-18/results.json
```

补齐 `.env.solana` 的两个服务地址后，先只读监控，再模拟。`source` 必须是自己维护的配置文件：

```bash
set -a
. ./.env.solana
set +a
./target/release/lp-maker-solana --config config/solana.toml monitor --seconds 120
./target/release/lp-maker-solana --config config/solana.toml run --once
nohup ./target/release/lp-maker-solana --config config/solana.toml run </dev/null >>solana-run.log 2>&1 &
```

实盘入口已有实现，但尚未通过用户服务端点和实际账户的完整只读/模拟检查。本次没有执行实盘命令、没有转币或广播交易。停止/重启时保留整个对应状态目录；使用 SIGTERM 停止后再启动同一配置，不能同时启动两个写实例。

## 验证记录

- 原有 150 项 Rust 测试通过，新增 12 项 Rust 测试通过：签名回复丢失、过期未决阻塞、重启核对、链上明确失败、映射幂等、EVM 状态隔离、bin 库存守恒、恢复比例、资金费幂等、配置杠杆保证金检查、APR 数量差分与未来数据隔离。
- 5 项 Node 测试通过：池身份、bin 对齐、签名原字节/payer 绑定、增量租金、gRPC 无业务数据重连与关闭。
- 公共 RPC 实际读取 pool/program/genesis/mints、active bin 和租金；官方 SDK 已构建 125-bin **未签名**初始化/分段增加指令，交易序列化大小低于 1232 字节。详情在研究目录 `read-only-verification.json`。
- 公共 RPC 的部分 swap 报价读取返回 403，完整 swap 模拟未验证；不把它标成交易成功。
- 公共 RPC + 官方 HL WS 的 paper 首次运行与保留状态重启通过，建立两层模拟仓位和模拟对冲，第二次 Hold，不重复建仓。
- Triton 专属 RPC/gRPC 地址未提供，认证、重连的真实服务端联调仍待补齐。gRPC 当前完成的是实现和本地生命周期测试。

参考：[官方 Meteora SDK](https://github.com/MeteoraAg/dlmm-sdk)、[Triton Dragon's Mouth](https://docs.triton.one/project-yellowstone/dragons-mouth-grpc-subscriptions)、[Hyperliquid Info API](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/info-endpoint)。
