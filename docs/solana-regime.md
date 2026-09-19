# SOL 趋势过滤 LP 候选

这是在六个月价格样本中盈利的**研究候选**。40% 仅是假设的 LP 到账手续费 APR，不能理解为保证利率。
真实双市场小时主模型：200 美元资金，净收益约 +1.64 美元；最后 62 天独立测试约 -1.66 美元，双倍执行成本约 -2.11 美元。因此默认只模拟，尚无稳定盈利证据。
完整数据口径、曲线、原始响应和费用拆分保存在本地 `backtests/solana/2026-09-18-six-month/REPORT.md`。整个 `backtests/solana/` 目录（包括历史行情、搜索参数和生成报告）不纳入 Git，克隆仓库不会带入这些文件。

## 与其他策略分开

- 配置：`config/solana-regime.toml`，状态：`data/solana-regime-paper`。
- 独立二进制：`lp-maker-solana`；Ethereum、Robinhood、Base 继续使用 `lp-maker`。
- Rust 决策：`src/solana/regime.rs`；Rust 研究模块：`src/solana/research/`。
- Solana RPC/gRPC、Meteora SDK、交易签名与恢复沿用 `src/solana/` 和 `adapters/solana/` 的隔离边界。
- 只有配置包含 `[regime]` 才启用本策略；原 `config/solana.toml` 保留旧行为。旧配置不增加身份字段，已有检查点仍可重启。
- 改变参数不能直接覆盖旧状态：经济指纹不一致会阻止启动。使用独立目录；不要删除旧持仓、订单或交易日志来绕过保护。

## 决策

预算为 LP 100 / Hyperliquid 40 / 租金及备用 60 美元。较宽 DLMM 账户约需 0.401 SOL 可退还租金，因此不能照搬原 20 美元备用额。备用资金的 SOL 币价风险没有计入主研究；这是实盘前必须重新核算的差异。

单层区间 `[P/1.15, P*1.15]`。入场需：收盘高于 EMA96、7 天涨幅至少 6%、EMA144 不下降、连续 6 个健康小时。区间内按实际 SOL 库存的 25% 开空，3 倍逐仓；调整数量不会再乘 3。

下跌趋势、0.8% 小时收益波动率、2.5 倍相对波动率、5% 小时振幅、3% 急跌、1% 池/合约基差、越出 LP 区间，任一退出条件成立就撤出 LP 并退出库存风险。具体阈值定义见报告和配置。组合回撤 5% 保持熔断。

退出后冷却 168 小时，还要再次满足趋势确认；不会一直下跌一直重建。首次进入也需要趋势确认。重启保留冷却与建仓历史，重新积累观察时序；重复行情消息不能重复增加健康小时。

## 本地检查与离线复算

以下复算命令要求本地已备好相应历史数据和 `accepted-parameters.json`。新机器可用 `scripts/solana/` 下载和研究脚本重新准备输入；Git 中保留策略代码与配置模板，不保存历史回测文件。

```bash
cargo build --release --locked
./target/release/lp-maker-solana --config config/solana-regime.toml check

./target/release/lp-maker-solana --config config/solana.toml research \
  --data backtests/solana/2026-09-18-six-month \
  --grid backtests/solana/2026-09-18-six-month/accepted-parameters.json \
  --apr 0.4 --output /tmp/solana-hourly-replay.json

./target/release/lp-maker-solana --config config/solana-regime.toml verify-research \
  --data backtests/solana/2026-09-18-six-month \
  --apr 0.4 --output /tmp/solana-fine-replay.json
```

复算不读取私钥、不连接交易接口。40% 假设手续费随实际区间内本金和时间入账，参与暂停/熔断；停仓时为零。线上运行仍按真实链上费用记账，不写入假设收益。

## Triton One 与模拟运行

提供商已确认是 Triton One。完整 RPC/gRPC 地址由账户套餐分配，不能从 token 推测。`.env.solana` 已留好 `LPMAKER_SOL_RPC_URL`、`LPMAKER_SOL_GRPC_URL` 和 `LPMAKER_SOL_GRPC_TOKEN`；不要把私密 URL 或 token 提交到 Git。

填入实际地址后，可运行只读监听与 paper：

```bash
npm ci --prefix adapters/solana --ignore-scripts
set -a
. ./.env.solana
set +a
./target/release/lp-maker-solana --config config/solana-regime.toml monitor --seconds 120
./target/release/lp-maker-solana --config config/solana-regime.toml run --once
```

这里没有 `--execute`，配置也是 paper。Triton 真实鉴权、完整换币与多笔建仓交易尚未验收，不应把离线回测通过当作线上执行验收。已有状态目录的同一配置重启不会新建一套账本。
