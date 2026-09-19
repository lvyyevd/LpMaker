# 状态持久化与启动对账

当前版本另有 `runtime_health.json`（决策完成时间与降级状态）、`hedge_residual.json`（未完全对冲的尾差）、`hl_identity.json`（已验证的签名账户关系）。`orders.json` 现在是日常台账，较早终态订单保存在 `order_archive/`，事件轮转到 `event_archive/`；容量与升级说明见 [运行可靠性与验收](production.md)。

执行状态存放在配置的 `state_dir`。200 美元模拟配置使用 `data/paper-200/`；其他配置使用各自目录。必须保留整个目录，不能只复制持仓文件。独占进程锁防止两个交易进程共用账本；只读 `status`/`monitor` 可以同时运行。

## 文件记录

| 文件 | 内容与作用 |
|---|---|
| `checkpoint.json` | 版本化的权威检查点：策略阶段、净值峰值、暂停时间、风控计数；paper 模式同时保存余额、LP、空单、待成交限价单及模拟费用 |
| `strategy.json` / `paper.json` | 检查点的兼容导出，供查看和监控；重启时从检查点修复，不覆盖权威检查点 |
| `nfts.json` | LP 层名称到 NFT token ID 的映射；确认 mint/burn 后更新 |
| `lp_history.json` | 曾建立过 LP 的持久证据；确认 mint、导入或核对到现有 LP 时记录，退出 LP 后仍保留 |
| `equity_baseline_basis.json` | 新收益基准采用的账本口径；统一账户修复前的旧基准标记不可比，不将漏计余额的修正显示成盈利 |
| `lp_inventory.json` | 最近启动核对的本地 NFT 与链上有效 NFT 清单及一致性 |
| `live_inventory.json` | 最近核对的实际 LP、钱包余额、永续仓位和保证金，包含账户原始响应与观察时间 |
| `account_snapshot.json` | Hyperliquid 原始快照及 `lpMakerCollateral` 资金口径；统一账户额外保留 spot 和 activeAssetData。即使检测到不允许的多仓/其他币种仓位也保留观察结果 |
| `orders.json` | 按 Cloid 索引的订单台账：账户、原始方向/价格/数量/TIF、OID、是否为策略对冲单、准备时间、当前状态、查询时间和交易所响应 |
| `open_orders.json` | 实际挂单列表和查询时间；手工/未知来源的订单也记录在这里 |
| `hedge_order.json` | 当前对冲订单的目标、方向、价格、数量和 Cloid；兼容导入旧版只含目标和 Cloid 的记录 |
| `pending.json` | 尚未确认结果的网络写操作，保留原始交易哈希或订单请求；先落盘再发送 |
| `evm_nonce.json` / `nonce.json` | EVM nonce 生命周期和 Hyperliquid 单调签名 nonce |
| `workflow.json` | 尚未完成的跨平台 LP 工作流 |
| `startup_reconciliation.json` | 最近启动的检查阶段、核对结果、订正原因、`ready` 或 `blocked` 状态 |
| `events.jsonl` | 追加式操作/决策/订单对账/持仓订正记录，保留历史启动结果 |

状态文件通过临时文件、`fsync`、原子重命名和目录同步写入，同一进程写入串行化。Paper 的阶段和资产在一个检查点中提交，避免阶段已经前进但余额仍是上一次的状态。文件损坏、版本不支持、遗留账本不完整时停止，不把它当成“新账户”重置资金。

## 启动顺序

1. 校验配置与状态目录绑定关系；加载检查点，必要时迁移旧版文件。保留 `Paused`、`Halted`、冷却起点和净值峰值。停机期间不是连续观察，健康计数和连续越界计数重新计数。
2. 实盘若有 `pending.json`，自动调用对账逻辑：链上按原哈希查询回执和确认；Hyperliquid 优先按已确认的 OID 查询，再用原 Cloid 交叉核对。成功确认的 mint 会补登记 NFT。禁止盲目重复发送。
3. 核对钱包身份、链 ID、latest/pending nonce 和 LP NFT。未登记 NFT、所有权变化或缺失映射会留下差异记录并阻止启动，不猜测它属于哪一层。
4. 读取实际账户与持仓，持久化挂单列表；刷新台账中的未决订单。部分成交使用交易所返回的原始/剩余数量和实际账户仓位，不按旧挂单总量重新补单。
5. 对仍然挂着、且能够证明属于本策略的遗留对冲单，撤销剩余委托后再次确认订单状态、读取实际仓位。随后由最新行情和策略重新计算目标。手工订单、触发单、其他来源订单不会自动接管或撤销；它们会阻止策略启动。
6. 将实际余额和仓位订正到 `live_inventory.json`；变化写入审计日志。如果保存的 `Active/Recovering` 阶段缺少应有 LP 层，进入 `Paused`，不会盲目重建。未完成 LP 工作流延用退出稳定币的恢复流程，回撤导致的 `Halted` 不会被降为 `Paused`。
7. 全部检查通过后写入 `ready`，才进入常规决策。执行过程中也会在决策前和动作后核对订单，并刷新实际持仓。

自动撤销旧策略单只在已授权的 **live 配置 + `run --execute`** 下发生；`status`、`monitor` 和单独的 `reconcile` 不会执行这一撤单流程。未决的链上交易仍按原哈希等待，不自动加价或换 nonce。

若显式运行过 `lp --execute retry-approval --hash <原哈希>`，`pending.json` 额外保存 `replacements` 中的已签名替换授权。原始 `hash` 和 nonce 不变，对账会检查原始及所有替换哈希，任一交易确认后才释放该 nonce。新旧交易使用同一 nonce、同一代币、spender、金额和 calldata；此入口仅支持 ERC20 授权，不能用于重放过期的 mint/swap。所有广播失败都保留 pending，包括明确的 RPC 费用拒绝；新版另保存 `last_broadcast_error`。操作步骤见 [EVM 费用与授权恢复](production.md#evm-费用与授权恢复)。

`unknownOid`、接口错误或无法识别的新订单状态都不是“订单没有成交”的证明。即使原请求已经过期，也保留未决记录并阻止重复下单；需核对交易所历史和实际资产后处理。不能简单删除 `pending.json` 绕过。

### Hyperliquid 已接受挂单，却暂时查询不到

交易所回执中的 `resting.oid` 会先持久化。订单查询优先用这个数字 OID，再回退到 Cloid；若两者暂缺，只允许用当前挂单列表中精确匹配 OID/Cloid 的记录确认仍在挂单，绝不按币种、价格或数量接管别人的订单。列表为空不代表未成交。

一次核对最多进行 5 轮只读查询，轮间等待 250、500、1,000、2,000 毫秒；HTTP 请求另受原有超时和重试限制。撤单后也要等到明确成交/撤销等终态，不能把短暂 `open` 或空列表当作撤单完成。每次查询写入 `order_status_query` 审计事件，已有回执和成交证据不会被 `unknownOid` 覆盖。

重试后仍未确认，会保留 `orders.json`、`hedge_order.json` 和已有 pending，以可重试的只读故障交给运行器。Robinhood/Base 连续 `run` 进入降级恢复循环，监控继续，完成对账前不继续策略交易；单次命令报错返回。身份不符或未知协议状态仍直接阻断。更新无需修改配置、检查点或清理状态，旧版本保存为 `unknown` 但带 OID 的订单也能重新核对。

**程序退出不等于交易所撤单。** 未成交挂单可能在停机后继续成交。更新重启必须保留原状态目录，启动时根据原订单和真实持仓订正。若存在 `workflow.json`，仍按原有规则处理未完成 LP 工作流，可能退出 LP 至稳定币；这次订单恢复修复不改变该规则。

## 首次建仓与暂停恢复

`strategy.entry_history` 保存在检查点中：新状态为 `initial`；一旦确认过任意一层 LP，变为 `established`，以后即使退出、清空 LP 或重启，也不会重新获得首次豁免。确认 mint 时还独立写入 `lp_history.json`，避免 mint 成功而检查点尚未更新的崩溃窗口。

- **首次建仓前**：允许跳过 6 小时冷却和累计 6 个新健康小时的等待，按完整 LP 预算建仓。首次授权、兑换等流程失败后，仍先核对 pending、挂单和实际余额，完成遗留工作流退出，再根据最新行情重新决策；不会直接重放旧 mint。
- **曾建立过 LP 后**：任意一层成功也算已建仓。暂停恢复仍要求配置中的 `cooldown_hours = 6` 和 `resume_healthy_hours = 6`，随后按 25% 分批恢复。日志的 `resume_wait` 展示剩余冷却秒数和健康小时进度。
- **始终保留**：快跌、单边下跌、波动突增、价差、历史 K 线不足、过期行情、未决交易和回撤停机限制。`Halted` 不因首次参数或重启而解除。

旧版 `Paused` 检查点没有首次记录，仅凭当前空仓无法区分“从未建仓”和“已经平仓”。因此默认迁移为 `legacy_unknown` 并保留冷却。**确认该状态目录从未成功建立过 LP** 时，可使用一次迁移参数：

```bash
cargo build --release --locked
./target/release/lp-maker --config config/local.toml run --execute --first-entry
```

这会实际运行实盘策略。先停止原策略进程，并沿用同一配置、状态目录及已设置的密钥环境变量。参数在完成链上与交易所对账后才生效；要求无未决操作、未完成工作流、现有 LP、WETH 库存或空单。已知 LP 历史（包括登记 NFT）优先，参数不能将 `established` 重置为 `initial`。迁移成功会写入审计日志并保存，后续运行无需再带该参数。不要删除状态文件或把冷却配置改成 0 来实现首次豁免。

空仓新增 LP 前会先读取 Hyperliquid 的实际账户权益及可用保证金。以 `LP 预算 × 建仓比例 / 杠杆 / 0.9` 检查保证金余量，并同时受配置的保证金预算限制；不足时记录 `entry_waiting_hedge_collateral`，不买入 WETH、不铸造 LP，保持等待并在下一轮重新检查。已有风险库存仍可退出到稳定币，执行对冲前仍再次检查保证金。200 美元配置的 LP 预算为 120、杠杆为 3 时，这项前置门槛约为 44.45 USDC；配置的 60 美元保证金应实际存入 Hyperliquid，仅填写配置不会产生资金。

## 查看与运行

```bash
# 文件状态、完整订单台账、最近启动对账结果；无需私钥
cargo run --locked -- --config config/paper-200.toml status
# 使用同一配置重新运行，即自动加载之前的检查点
cargo run --locked -- --config config/paper-200.toml run
# 单独查询未决交易结果，不重发旧请求
cargo run --locked -- --config config/local.toml reconcile
```

`status` 隐去 pending 内的原始已签名交易和签名字段。状态文件仍含账户和交易信息，应保留在本机，不提交版本库。备份应在进程停止后复制整个状态目录。

订单台账保存本程序订单的请求与查询结果，并通过 `events.jsonl` 保留状态变化；它不是交易所全历史成交的独立数据库。程序停机时不会持续采集事件，重启后使用可查询的订单状态和实际资产补正；接口已无法返回的历史不会编造。Paper 仍采用原有采样成交模型，不会声称知道停机期间的逐笔成交。

协议依据：[Hyperliquid Info API 的订单状态查询](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/info-endpoint)。
