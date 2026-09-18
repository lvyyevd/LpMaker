# 状态持久化与启动对账

当前版本另有 `runtime_health.json`（决策完成时间与降级状态）、`hedge_residual.json`（未完全对冲的尾差）、`hl_identity.json`（已验证的签名账户关系）。`orders.json` 现在是日常台账，较早终态订单保存在 `order_archive/`，事件轮转到 `event_archive/`；容量与升级说明见 [运行可靠性与验收](production.md)。

执行状态存放在配置的 `state_dir`。200 美元模拟配置使用 `data/paper-200/`；其他配置使用各自目录。必须保留整个目录，不能只复制持仓文件。独占进程锁防止两个交易进程共用账本；只读 `status`/`monitor` 可以同时运行。

## 文件记录

| 文件 | 内容与作用 |
|---|---|
| `checkpoint.json` | 版本化的权威检查点：策略阶段、净值峰值、暂停时间、风控计数；paper 模式同时保存余额、LP、空单、待成交限价单及模拟费用 |
| `strategy.json` / `paper.json` | 检查点的兼容导出，供查看和监控；重启时从检查点修复，不覆盖权威检查点 |
| `nfts.json` | LP 层名称到 NFT token ID 的映射；确认 mint/burn 后更新 |
| `lp_inventory.json` | 最近启动核对的本地 NFT 与链上有效 NFT 清单及一致性 |
| `live_inventory.json` | 最近核对的实际 LP、钱包余额、永续仓位和保证金，包含账户原始响应与观察时间 |
| `account_snapshot.json` | 实际 Hyperliquid 账户原始快照；即使检测到不允许的多仓/其他币种仓位也保留观察结果 |
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
2. 实盘若有 `pending.json`，自动调用原有对账逻辑：链上按原哈希查询回执和确认；Hyperliquid 按原 Cloid 查询。成功确认的 mint 会补登记 NFT。禁止盲目重复发送。
3. 核对钱包身份、链 ID、latest/pending nonce 和 LP NFT。未登记 NFT、所有权变化或缺失映射会留下差异记录并阻止启动，不猜测它属于哪一层。
4. 读取实际账户与持仓，持久化挂单列表；刷新台账中的未决订单。部分成交使用交易所返回的原始/剩余数量和实际账户仓位，不按旧挂单总量重新补单。
5. 对仍然挂着、且能够证明属于本策略的遗留对冲单，撤销剩余委托后再次确认订单状态、读取实际仓位。随后由最新行情和策略重新计算目标。手工订单、触发单、其他来源订单不会自动接管或撤销；它们会阻止策略启动。
6. 将实际余额和仓位订正到 `live_inventory.json`；变化写入审计日志。如果保存的 `Active/Recovering` 阶段缺少应有 LP 层，进入 `Paused`，不会盲目重建。未完成 LP 工作流延用退出稳定币的恢复流程，回撤导致的 `Halted` 不会被降为 `Paused`。
7. 全部检查通过后写入 `ready`，才进入常规决策。执行过程中也会在决策前和动作后核对订单，并刷新实际持仓。

自动撤销旧策略单只在已授权的 **live 配置 + `run --execute`** 下发生；`status`、`monitor` 和单独的 `reconcile` 不会执行这一撤单流程。未决的链上交易仍按原哈希等待，不自动加价或换 nonce。

`unknownOid`、接口错误或无法识别的新订单状态都不是“订单没有成交”的证明。即使原请求已经过期，也保留未决记录并阻止重复下单；需核对交易所历史和实际资产后处理。不能简单删除 `pending.json` 绕过。

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
