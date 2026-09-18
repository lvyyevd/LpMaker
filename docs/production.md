# 运行可靠性修复与验收

本轮针对 2026-09-18 审查中的六类缺陷。默认仍是 paper。测试只使用本地模拟服务和公开固定签名测试向量，不加载用户私钥。

## 已修改行为

| 场景 | 新行为 |
|---|---|
| Maker 部分成交，开仓尾差小于最低金额或数量精度 | 下单意图落盘前校验；记录 `hedge_residual.json`，下一轮按实际持仓重算。不为凑最低金额扩大仓位，不生成未发送的订单编号。紧急对冲不再受普通 deadband 的 12 美元门槛限制，但仍遵守最低金额。 |
| API 钱包与配置账户错配或授权到期 | 启用签名时及每次写操作前核对主账户、API agent、有效期和子账户/金库归属。失败不发送交易；身份记录到 `hl_identity.json`。 |
| 读取超时、断开、429/5xx | 只读接口有限重试；仍失败则标记 degraded，默认 5 秒后重新加载检查点、核对 pending、实际持仓和订单，再恢复决策。遗留策略挂单按恢复流程撤销。 |
| 下单或广播响应丢失 | 保留原哈希/Cloid 和 pending，不自动重发。异常中的“超时”不意味着可以按只读错误重试。 |
| 意图落盘后、调用网络发送前中断 | 新版持久化 `prepared` 能证明尚未发起写入，可标记 `not_submitted` 后重新计算。发送前先落盘 `submitting`；该状态和旧版无标记记录仍按结果未知处理。 |
| 查询耗时导致行情陈旧 | 观测批次默认 45 秒截止，IO 完成后重新取时间检查时效。LP 建仓前、HL 对冲签名前、EVM 审批/估算完成后再次检查报价时间。 |
| 无持仓时杠杆设置响应丢失 | 使用 `activeAssetData` 核对账户、币种、杠杆值和模式，匹配才清除 pending。启动时已有目标配置则不重复设置。 |
| 事件和订单台账增长 | 事件按容量轮转；终态订单先持久化到不可变归档，再从日常台账移除。未知订单及存在 pending 时的记录不归档，归档 Cloid 禁止复用。 |

20 美元目标、成交 15 美元后剩余约 5 美元的场景已纳入回归测试。**保留尾差意味着仍有少量未完全对冲的敞口**。状态查询可查看金额、目标和实际仓位；达到可交易条件后按最新目标处理。

`hyperliquid.account` 填主账户，子账户/金库填 `hyperliquid.vault`，两者不能相同。API agent 需要能通过 `extraAgents` 验证的具名授权，且距离到期超过一分钟；不能确认授权时停止写操作。主账户本人签名也核对账户角色，每次写操作前重新检查授权。

## 新增配置

旧配置缺少这些字段时采用默认值；本机 `config/local.toml` 未被覆盖。

```toml
# 添加到已有 [liquidity]，不要重复创建该表
max_quote_age_seconds = 60

[runtime]
read_retry_seconds = 5
observation_timeout_seconds = 45

[storage]
event_segment_bytes = 16777216
event_retained_segments = 32
terminal_orders_keep = 1000
order_archive_max_bytes = 268435456
min_free_bytes = 268435456
```

`event_archive/` 默认保留最近 32 个事件分段，每个新分段最多 16 MiB，另有当前 `events.jsonl`。超过保留数量会删除最老分段；如需永久保存完整事件历史，应在删除前进行外部归档。升级前已超大的文件整体轮转，不自动截断旧日志。

`orders.json` 保留未决记录和最近 1000 条可归档终态记录；`order_archive/<cloid>.json` 保存较早终态记录及结果。当前对冲指针引用的记录不会提前移除。先同步归档、再提交台账变更，中断后可重复完成归档。

订单归档不自动删除，用于审计及阻止编号复用。默认容量达 80% 时记录警告，达到 256 MiB 上限时阻止归档并停止策略；应保留历史、增加容量或迁移完整状态目录。不要在线修改归档文件。磁盘可用空间低于 256 MiB 时不准备新交易，已有结果的记录和对账仍可使用预留空间。

## 统一账户资金读取

程序通过 `userAbstraction` 自动识别账户模式，每次资金快照前后核对模式，避免切换期间混用两个账户口径；不自动修改交易所账户模式，也不发起划转。

| 模式 | 策略净值的 Hyperliquid 部分 | 新增空单可用保证金 |
|---|---|---|
| `disabled` / `default` / `dexAbstraction` | 原生 `clearinghouseState.marginSummary.accountValue` | 原生 `withdrawable` |
| `unifiedAccount` | `spotClearinghouseState` 中 token 0、coin USDC 的 `total` | 对冲币种 `activeAssetData.availableToTrade[1]`，同时受 USDC `total - hold` 限制 |

统一账户不再把原生永续接口的 0 当成实际资金为 0。保留原始响应，并额外提供 `lpMakerCollateral`，包括账户模式、来源、币种、USDC 权益和买卖两侧可用金额。共享账本只计一次；不再叠加传统永续 `accountValue` 或重复叠加仓位 PnL，不把 USDH、HYPE 等余额当成 USDC。当前仍面向专用原生永续对冲账户；HIP-3 和 `portfolioMargin` 的借贷、多资产估值不在支持范围，后者会明确报错。账户模式未知、切换中、必要数据缺失或数字无效时停止新增，不回退成零余额或猜测可用资金。

建仓前资金检查、实际新增对冲单检查和持仓净值均使用此口径。配置的保证金预算仍限制头寸，账户里有 79.60 USDC 不会自动把策略保证金预算从 60 改为 79.60。短仓检查使用卖出方向的额度，不能拿买入方向的额度替代。

原生 WS 增加 `spotState`、各监听币种的 `activeAssetData`。每 60 秒中文状态摘要单独显示已核对 USDC 权益与开空可用保证金（原始报告字段 `collateral`），账户数据仍每 30 秒刷新；原始 WS 事件保存在 `account_ws` 中，不能覆盖经过账户模式核对的 REST 资金快照。断线清空 WS 缓存，REST 失败保留旧观察时间与错误，不假装数据已刷新。

修复会把原先漏计的统一账户 USDC 计入组合净值。为避免将余额修正显示成策略盈利，新建收益基准同时记录 `equity_baseline_basis.json`；旧基准或不同口径基准不被覆盖。统一账户旧基准无法比较时，日志标记 `accounting_basis_changed`，`equity_change_usd` 为 `null`。当前净值仍显示，风险高水位和暂停状态仍保留。

只读核对，无需签名私钥：

```bash
./target/release/lp-maker --config config/local.toml info account
```

依据：[官方账户模式](https://hyperliquid.gitbook.io/hyperliquid-docs/trading/account-abstraction-modes)、[现货余额接口](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/info-endpoint/spot)、[永续账户与 activeAssetData](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/info-endpoint/perpetuals)。

2026-09-18 验证：126 项测试、fmt、Clippy 和 release 编译通过。本地 mock 覆盖统一账户余额、方向额度、冻结金额、标准账户、模式切换、缺失数据以及本地签名下单路径。新版 release 程序只读查询用户账户，识别到 `unifiedAccount`、USDC 权益 79.60、ETH 卖出侧可交易保证金 79.60；35 秒官方 WS 监听收到 `spotState` 和 `activeAssetData`，正常退出。没有读取真实私钥或发送真实交易、划转。

## EVM 费用与授权恢复

新版读取最新区块 `baseFeePerGas` 和节点建议费用，预留比例在 `[liquidity]` 配置：

```toml
gas_fee_buffer_bps = 10000   # 费用上限额外预留 100%，即估算值的 2 倍
gas_limit_buffer_bps = 2000 # gas 用量额外预留 20%，即估算值的 1.2 倍
```

`100 bps = 1%`。费用预留允许 1–40000 bps，用量预留允许 1–10000 bps，均不能为零。旧配置缺少字段时使用上述默认值。调整预留比例不会重置现有仓位、pending 或策略阶段；单笔费用预算 `max_gas_native` 仍属于需核对的经济参数。

设 `m = 1 + gas_fee_buffer_bps / 10000`。支持 EIP-1559 的链发送 type-2 交易，费用上限为 `max(ceil(m × baseFee) + priorityFee, ceil(m × gasPrice))`。零 priority fee 合法；仅在节点明确不支持 priority-fee RPC 时，由 gasPrice 和 baseFee 推导。其他查询错误阻止发送。无 base fee 的旧链发送 legacy 交易，gasPrice 为 `ceil(m × 节点建议值)`。

新交易的 gas limit 为 `ceil(eth_estimateGas × (1 + gas_limit_buffer_bps / 10000))`，向上取整保证小额估算也包含余量。日志记录原始 gas 估算、两种预留比例、最终 gas limit 和费用上限。同 nonce 授权恢复使用当前配置计算新的费用上限，gas limit 保持原交易值，并重新检查其能否覆盖当前模拟用量。

EIP-1559 的上限留出涨价空间，实际费用取决于成交区块的基础费和小费，并非一定支付整个上限。Legacy 交易则实际使用所填 gasPrice。两种路径都按 gas limit × 费用上限检查 `max_gas_native` 和钱包 ETH 余额；余额或预算不足，在签名、记录 intent 和广播前停止。费用余量不能保证任意突发上涨时都成功，也不包括所有 L2 的额外费用模型。

旧版直接将一次 `eth_gasPrice` 结果用于 legacy 交易，在广播前基础费略微上涨就可能出现 `max fee per gas less than block base fee`。失败授权的 pending 不能删除，也不能换新 nonce 重做。

确认旧运行进程已停止，备份完整状态目录，更新代码并重新编译。在原配置、原 state_dir 下，加载本机私钥环境变量后显式执行：

```bash
cargo build --release --locked
# 会签名并发送授权恢复交易；填 pending.json 中的原始 hash。
./target/release/lp-maker --config config/local.toml lp --execute retry-approval --hash <原始交易哈希>
```

命令首先验证保存的签名、哈希、钱包、链、nonce、代币、spender、金额和 calldata；检查所有历史哈希的回执和钱包 nonce。若之前的交易已经确认，只对账；若 nonce 被未知交易占用或消费，保留 pending 并停止。确需发送时，仅在同一个 nonce 上提高费用，保留原授权内容和 gas limit，重新模拟并核对费用预算。新上限和小费至少比上一尝试提高 25%（加 1 wei 处理舍入），仍可能被节点的替换规则拒绝。最多允许八次显式替换，不自动循环加价。

原始和替换交易都先持久化再广播；广播结果丢失、节点拒绝或进程崩溃不释放 nonce。`reconcile` 以及下一次 `run --execute` 会检查全部候选哈希。授权恢复确认后，再运行原来的策略命令；已有 LP 工作流继续按恢复规则对账、退出，不直接重做旧建仓。曾建立过 LP 的账户仍须冷却；首次建仓前的等待豁免及旧状态迁移见 [首次建仓与暂停恢复](recovery.md#首次建仓与暂停恢复)。该恢复命令本身只处理授权。

费用修复和可配置预留新增 15 项本地回归测试，完整 102 项测试及 fmt / Clippy 均通过。覆盖原日志费用上涨、旧版 legacy 授权恢复、重复费用拒绝、广播响应丢失后重启、原交易抢先确认、nonce 状态写入中断、未知外部 nonce、预算限制和损坏签名拒绝对账；也验证预留比例确实进入签名、整数向上取整、旧配置默认值及状态绑定兼容。签名使用公开固定测试向量，交易请求仅发送到本地 mock RPC；没有使用真实账户签名或广播。

依据：[EIP-1559 费用定义](https://eips.ethereum.org/EIPS/eip-1559)。

## 健康检查与进程管理

```bash
cd /Users/wangzheng/lvyyevd/code/rustcode/LpMaker
cargo run --locked -- --config config/paper-200.toml check
cargo run --locked -- --config config/paper-200.toml run
```

另一终端检查：

```bash
cd /Users/wangzheng/lvyyevd/code/rustcode/LpMaker
cargo run --locked -- --config config/paper-200.toml health --max-age-seconds 120
cargo run --locked -- --config config/paper-200.toml status
```

`health` 只读本地状态，不加载私钥、不调用交易接口。仅当状态为 running，且最近一轮策略决策及操作已经完成、未超过时效阈值，才返回退出码 0。starting、recovering、degraded、blocked、stopped 或决策停滞均返回非零。外部监控应同时检查进程和该退出码，启动报告的 ready 不是实时健康证明。

行情不满足条件而保持 Paused 可以是健康运行。LP 多步工作流可能超过检查窗口，应结合 pending 调查；不能因健康检查失败就删除状态或盲目平仓。

运行、独立监控和 watch 支持 SIGINT/SIGTERM，停止保留实际仓位与未决记录，重启先对账。`deploy/lpmaker-paper.service` 为未安装的 Linux paper 服务示例，带重启限速。本轮没有安装服务、建立定时任务或接入外部通知。同一资金账户只允许一个写入实例，即使 state_dir 不同也不能并行交易。

## 升级与剩余验收

本轮验证：`cargo build --locked`、`cargo fmt --all -- --check`、`cargo clippy --locked --all-targets -- -D warnings` 通过；`cargo test --locked` 共 87 项通过，其中 18 项为本轮新增回归测试。另启动了真实 paper CLI 进程，所有端点均指向本地阻塞模拟服务，验证 SIGTERM 正常退出、状态写为 stopped、不产生 pending，停止后的 health 返回非零。此项验证不涉及真实交易或公共行情接口。

本地日志位于 `data/fix-verification-20260918/`（Git 忽略），包括测试、构建、Clippy 和 CLI 冒烟验证结果。

- 停止旧进程，完整备份状态目录，包括归档；使用原配置和原 state_dir 启动新版，不为规避 pending 创建新目录。
- 新字段有默认值，新增报价时效字段不改变旧经济参数指纹；RPC/监控参数修改不会清空风险阶段。
- 旧版 unknown 缺少“确定未发送”的证据，仍需核对真实历史。本轮不批量清除历史未知记录，`submitting` 也不能因超时而认定没有成交。
- 未完成 LP 工作流在只读故障恢复后，沿用退出稳定币的恢复流程，不重放旧 mint；Halted 状态继续保留。
- 自动化测试覆盖本轮边界及原有功能。尚未完成目标服务器 72 小时持续测试、真实资金闭环、独立清算距离处置、第三方审计和外部告警接入，仍不标记为已验收的无人值守实盘系统。

协议依据：[userRole / vaultDetails](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/info-endpoint)、[activeAssetData](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/info-endpoint/perpetuals)、[官方 SDK extra_agents](https://github.com/hyperliquid-dex/hyperliquid-python-sdk/blob/master/hyperliquid/info.py)。
