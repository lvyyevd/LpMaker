# 验证记录

## 状态持久化与启动对账：2026-09-17

- `cargo fmt --all -- --check`、`cargo clippy --locked --all-targets -- -D warnings`：通过。
- `cargo test --locked`：69 项测试通过，新增 11 项恢复测试。
- 新增覆盖：检查点恢复余额/待成交单/风险阶段、修复陈旧导出文件、旧版迁移、损坏或缺失账本拒绝重置、部分成交和撤单台账、手工/未知订单阻塞、未知返回值、并发原子更新、按 Cloid 解决回执丢失、过期但未知的订单仍保留 pending、实际账户持仓订正及审计记录。回撤 `Halted` 在未完成工作流恢复后仍保持停机。
- 公开行情下的独立 200 美元 paper 重启验证：首次 `Warmup → Active / Deploy` 创建两层 LP，退出后再次启动为 `Active / Hold`；两层的区间、流动性数量、钱包 USDG 和累计模拟换币成本保持不变，没有重复建仓。检查点时间推进，启动对账报告为 `ready`。
- 运行证据位于忽略目录 `data/verification-recovery/`：`first-start.log`、`restart.log`、`checkpoint-before-restart.json`、`summary.json`、`tests.log`。

本次没有加载用户私钥或发送实盘交易。订单恢复测试使用本机只读 Info API 模拟服务，出现交易写请求即失败；遗留订单撤单的真实签名/成交竞态仍未用真实资金验证。文件和自动恢复行为见 [状态持久化与启动对账](recovery.md)。

## 监听与生命周期改造：2026-09-17

- `cargo fmt --all -- --check`：通过。
- `cargo test --locked`：58 项测试全部通过（原有 43 项，加 15 项生命周期与恢复测试）。
- `cargo clippy --locked --all-targets -- -D warnings`：通过。
- `config/local.toml`、`config/paper-200.toml` 配置校验：均通过，均为 paper 模式。
- Hyperliquid 官方 WSS：收到 ETH、BTC、SOL 行情，以及已配置账户的订阅和账户快照。
- Robinhood PublicNode WSS：收到 newHeads 和目标池事件；另将普通 `rpc_url` 切换为 WSS，成功读取完整池快照（区块 65403041）。明文 `ws://` 通过本地服务测试。
- 隔离的 200 美元 paper 流程：初次运行创建两层模拟 LP，后续成功恢复状态；最终连续运行约 105 秒，完成 5 次策略决策，未出现 WARN/ERROR，Ctrl-C 后回收监听和刷新任务。
- 最终运行输出 4 次 Hyperliquid 状态，相邻间隔约 29.98–30.001 秒；7 次 Robinhood 状态，相邻间隔约 14.961–15.013 秒。输出包含两层 LP 区间和位置、价格、模拟净值、费用口径，以及完整的 300 秒确认成交量窗口。
- 本地故障测试覆盖断线重订阅、握手时退出、Pong 不掩盖业务空闲、订阅 ID 映射、并发交易串行分配 nonce、广播响应丢失后阻止重复发送、回滚消耗 nonce、崩溃恢复、钱包身份隔离、配置迁移、日志去重和重组恢复。生产空闲阈值 300 秒有断言，同一实现的故障测试将其缩短到 2 秒。

首次公共节点测试发现免费 PublicNode 历史日志请求被 403 拒绝，因此增加 `archive_rpc_url`，仅历史补数使用 Robinhood 官方 RPC。历史补数与当前价格/仓位刷新分开，慢查询不再阻塞定时状态输出；数据时间和完整性会单独显示。详细设计与限制见 [监听与生命周期](lifecycle.md)。

最终运行摘要与日志保存在被 git 忽略的 `data/verification-lifecycle/summary.json`、`paper-complete.log`、`rpc-wss.log` 和 `tests.log`。该目录也保留了修复前的诊断日志，不能把早期失败记录误认为最终运行结果。

本次未读取用户 `.env` 或真实私钥，未向真实网络发送签名交易。本地签名和广播测试仅使用公开固定测试私钥及回环模拟 RPC。真实 Hyperliquid 下单成交、链上授权/mint/burn/swap 仍未使用真实资金验证；paper 收益未模拟 LP 手续费和 gas，不是收益预测。

`config/local.toml` 保留原有资金参数；200 美元测试使用独立的 `config/paper-200.toml`，状态写入 `data/paper-200/`，不会混用原有账本。

## 初始版本

2026-09-17，macOS，本机 Rust / Cargo 1.97.0。

- `cargo fmt --all -- --check`：通过。
- `cargo test --locked --offline`：43 项测试全部通过。
- `cargo clippy --locked --offline --all-targets -- -D warnings`：通过。
- Hyperliquid ETH L2 Book HTTP 查询：通过。
- ETH、BTC、SOL WebSocket 8 秒监听：收到 56 个事件，包含订阅回执、allMids、L2Book、trades、activeAssetCtx、candle 和心跳。
- Robinhood Chain RPC 与官方 V3 部署校验、目标池快照：通过。
- 池监听实际解码 28 个 Swap、2 个 Burn、2 个 Collect、1 个 Mint，已确认区块游标落盘。
- 真实行情单次 paper 决策：创建两层模拟 LP，状态与账本落盘；没有签名或广播真实交易。
- 336 根完整小时 K 线回放：完成，覆盖 Warmup、Paused、Recovering 路径。这是功能验证，不能当作收益预测。

测试覆盖签名官方向量、ABI selector、精度、LP 边界/Delta、负 tick、反向代币顺序、交易内嵌拒绝、未知操作落盘、进程锁、并发 nonce、慢跌/快跌/波动暂停、价差、陈旧数据、观察间断、恢复比例、回撤锁定、部分层重建的对冲保留、Maker 非即时成交、恢复加仓不改变区间及全余额兑换的舍入边界。

**未验证**：使用真实资金的合约成交、授权/mint/increase/remove/swap 广播、跨平台故障演练、强平模型和长期收益。本次没有读取真实私钥或发送实盘操作。

本地验证数据保存在被 git 忽略的 `data/verification-20260917/`。默认 `data/paper/` 已腾空，用户首次启动从全新的模拟账本开始。
