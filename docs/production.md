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
