#!/usr/bin/env python3
"""Inspectably derive the engineering research note and figures from saved outputs."""
import csv
import datetime as dt
import hashlib
import json
import pathlib

ROOT=pathlib.Path(__file__).resolve().parents[2]
p=ROOT/'backtests/solana/2026-09-18-six-month'
j=json.loads((p/'accepted-hourly.json').read_text())
f=json.loads((p/'accepted-fine.json').read_text())
m=json.loads((p/'manifest.json').read_text())
h=j['full']; selected=j['selected']['parameters'];fine=f['full'];q=j['holdout']
start=dt.datetime.fromtimestamp(j['start_ms']/1000,dt.timezone.utc)
end=dt.datetime.fromtimestamp(j['end_ms']/1000,dt.timezone.utc)
raw=json.loads((p/'pool_1h.json').read_text())
rows=[('真实双市场小时价格，40% APR',h),('小时价格，执行成本翻倍',j['double_cost']),('小时价格，30% APR',j['apr_30_percent']),('最后62天独立起始测试',q),('5分钟池价格 + 合约代理',fine),('5分钟低点优先情景',f['low_first_stress']),('5分钟低点优先 + 双倍成本',f['double_cost_low_first'])]
with (p/'summary.csv').open('w',newline='') as fp:
 w=csv.writer(fp);w.writerow(['scenario','net_pnl_usd','lp_fees_usd','drawdown_pct','active_hours','operations'])
 for name,x in rows:w.writerow([name,x['pnl'],x['lp_fees'],x['max_drawdown_pct'],x['active_hours'],x['operations']])
with (p/'hourly-equity.csv').open('w',newline='') as fp:
 w=csv.DictWriter(fp,fieldnames=h['equity_curve'][0].keys());w.writeheader();w.writerows(h['equity_curve'])
bins=2*__import__('math').ceil(__import__('math').log1p(selected['width'])/__import__('math').log1p(.0004))+1
rent=(41859200+max(0,bins-70)*568960)/1e9
max_price=max(x['high'] for x in raw)
# Provider data and executable inputs are hashed; no private configs or env files are read.
receipt={"window":[start.isoformat(),end.isoformat()],"pool_rows":m['quality']['pool_1h_']['rows'],"hourly_missing":m['quality']['pool_1h_']['missing'],"fine_missing":m['quality']['pool_5m_']['missing'],"primary_definition":"hypothetical 40% fee APR; observed hourly pool/perp prices and funding", "candidate_status":"six-month historical profit only; independent final62d and hourly doubled-cost tests fail", "position_bins":bins,"position_rent_sol":rent,"reserve_at_max_historical_price_usd":(rent+.034)*max_price,"inputs_sha256":{name:hashlib.sha256((p/name).read_bytes()).hexdigest() for name in ['manifest.json','pool_1h.json','pool_5m.json','hyperliquid_1h.json','hyperliquid_5m_observed.json','funding.json','accepted-parameters.json','accepted-hourly.json','accepted-fine.json']},"code_sha256":{str(x.relative_to(ROOT)):hashlib.sha256(x.read_bytes()).hexdigest() for x in sorted((ROOT/'src/solana/research').glob('*.rs'))},"provider":"Triton One confirmed by user; full RPC/gRPC endpoints still required; no live transactions"}
(p/'evidence.json').write_text(json.dumps(receipt,ensure_ascii=False,indent=2))
table='\n'.join(f"| {name} | {x['pnl']:+.4f} | {x['lp_fees']:.4f} | {x['max_drawdown_pct']:.3f}% |" for name,x in rows)
preview='\n'.join(f"| {dt.datetime.fromtimestamp(x['timestamp'],dt.timezone.utc).isoformat()} | {x['open']:.4f} | {x['high']:.4f} | {x['low']:.4f} | {x['close']:.4f} |" for x in raw[:3])
text=f'''# SOL / Meteora 六个月策略研究：40% 假设手续费 APR

**找到六个月样本内盈利候选，未找到足以证明稳定盈利的方案。默认仅模拟。**

时间为 **2026-03-18 08:00 UTC 至 2026-09-18 08:00 UTC（不含终点）**，共184天；北京时间两端均为16:00。池子为 `5rCf1DM8LjKTw4YqhnoLcngyZYeNnQqztScTogYHAS6`，SOL/USDC，bin step 4 bps。

## 主要结果

总预算200美元，调整为 **LP100 / Hyperliquid40 / 租金与备用60**。主结果使用全覆盖的真实池子与真实 Hyperliquid **小时价格**。六个月模型净收益 **{h['pnl']:+.4f}美元（{h['pnl']/2:.3f}%）**，其中假设 LP 手续费 **{h['lp_fees']:.4f}美元**，其余库存、对冲、资金费和列示执行成本合计 **{h['price_and_hedge_pnl']:+.4f}美元**。

主模型持有LP约 **{h['active_hours']:.0f}小时 / {h['active_hours']/24:.2f}天**，有 **{h['operations']}次建/撤仓工作流，约{h['operations']//2}轮进出**，大部分时间持有报价币。它保留 SOL 方向性敞口，不是完全 Delta 中性的手续费套利。

**最后62天从空仓独立起始测试亏损 {q['pnl']:.4f}美元；主回测成本翻倍后亏损 {j['double_cost']['pnl']:.4f}美元。不能据此保证实盘获利，也不能把40% LP年化理解为整个200美元账户年化40%。**

| 场景 | 净收益 USD | 假设LP费 USD | 模型最大回撤 |
|---|---:|---:|---:|
{table}

最后62天独立测试会重新初始化资金、冷却和仓位；它不是完整六个月曲线的末段差值，不能与此前各段收益相加。低点优先是一个可行情景，不是所有逐笔路径的最坏上界。

![六个月价格、净值和敏感性](six-month.png)

## 收益如何入账

每个价格区间的手续费 = **该段实际LP本金 × 0.40 × 在区间内小时数 ÷ 8760**。本金取区间起止市值较低者；一根观察K线的高低点越出LP范围，则该根不计手续费。资金未部署、退出、数据缺口期间均为零；只在下一个决策前入账，参与净值与熔断。费用不自动追加到LP本金，不按APY复利计算。

40%是用户指定的**LP到账手续费 APR 假设**，不是该池历史测得的个人收益；未再额外扣一次协议分成。研究已扣：合约执行费及滑点 **{h['hedge_cost']:.4f}美元**，换币成本 **{h['swap_cost']:.4f}美元**，链上工作流 **{h['operation_cost']:.4f}美元**。历史资金费贡献 **{h['funding']:+.4f}美元**。

模型按合约 taker 4.5 bps + 3 bps滑点计费，没有用“所有 maker 挂单都能成交”来提高收益；线上仍复用 maker 优先、撤单核对、紧急IOC的执行器。建仓换入WSOL后，临时全额对冲、添加LP、再降低对冲比例的额外成交费已计入。换币34bps、每次LP工作流0.10美元为假设；期末强制撤LP、卖出库存、平空也计费。

## 固定参数与退出条件

- 单个主区间：价格P附近 **[P/1.15, P×1.15]**，约下方13.04%、上方15%，按bin对齐，不频繁追价。
- 入场：已闭合小时收盘价高于EMA96；过去168小时上涨至少6%；EMA144不下降；通过波动、急跌和价差检查；连续观察6个符合条件的闭合小时。首次也不跳过趋势检查。
- 区间内：做空 **LP实际SOL库存的25%**，钱包WSOL余量另外对冲；3倍逐仓仅决定保证金，不把对冲数量再乘3。
- 下跌退出：收盘价低于EMA48、EMA48低于EMA144且两者下降；或收盘低于EMA144的99.5%且EMA144下降；任一成立即退出。
- 大波动退出：最近24个小时收益的标准差超过0.8%，或最近6小时/此前168小时波动率比超过2.5，或上一小时高低振幅超过5%。
- 急跌/实时保护：上一完整小时跌幅超过3%，或当前价相对上一小时收盘跌超3%；小时内绝对位移超过5%也退出。池/合约价差超过1%退出。
- 任意一边越界：退出并等待；不立即在新价格重建。退出后冷却 **168小时（7天）**，冷却结束还必须重新满足趋势条件和6个健康小时。
- 组合高水位回撤5%：保持熔断。重启不会清掉冷却、建仓历史或熔断；重复WS消息不会增加健康小时。

## 数据质量与适用边界

- 4416根池小时K线、4872根Hyperliquid小时K线（含19天预热）、4416个资金费小时：无缺失、重复或无效OHLC。资金费原始结算比整点晚数毫秒，按所属小时核对完整性，保留原始时间戳。结算归到决策前已有的空单；费率从不作为未来可知的信号。
- 池5分钟数据52991根，缺1根：2026-08-15 20:00 UTC。未补造价格；跨缺口不计费，并在下一次可观察价格退出后重新冷却。
- **五分钟 Hyperliquid 只有4995根真实记录，另47996根使用该小时开盘已知基差乘池价格的代理。约90.6%是代理，所以五分钟结果仅作敏感性检查，主结论必须用完整真实双市场小时数据。**
- 小时模型看不到线上15秒响应；五分钟低点优先也不是逐笔成交。DLMM账本为离散等值bin近似，线上按SDK Spot配置和真实仓位余额执行；历史bin竞争份额和maker排队仍不可得。
- 可退还租金未作为费用消耗。此区间约{bins} bins，按本次RPC租金表，position约 **{rent:.6f} SOL**；加0.03 SOL gas底仓与0.004 SOL账户余量，在样本最高价下约 **{(rent+.034)*max_price:.2f}美元**，60美元预留满足初始预算检查。实际签名前仍需模拟，未知bin array创建费会阻止超预算交易。
- **备用资金按固定美元估值；未计实际备用/租金SOL币价变化、兑换/退款价差、节点订阅费、交易失败/延迟/MEV和USDC脱锚。** 没有独立的逐仓清算模拟器。这个微利候选对遗漏成本敏感，不能称为已验证的线上净利润。
- 总共仅约{h['operations']//2}轮进出，交易样本很少。多轮开发中查看过不同候选的后段表现；最后62天未进入最终参数打分，但不是完全盲测。所有中间结果保留，不能称为无偏样本外验证。

可核对源数据前三行（池小时，UTC）：

| 时间 | 开 | 高 | 低 | 收 |
|---|---:|---:|---:|---:|
{preview}

## 工程实现与复现

- `src/solana/regime.rs`：独立SOL策略状态机与风控，RPC/签名之外的纯Rust计算。
- `src/solana/research/data.rs`、`simulation.rs`、`search.rs`、`verification.rs`：分别负责数据校验、资金与手续费账本、选参、细粒度敏感性。
- `config/solana-regime.toml`：固定候选，默认paper，独立状态目录 `data/solana-regime-paper`。不修改已有 `config/solana.toml` 或EVM配置。
- `src/solana/runner.rs`：只有显式 `[regime]` 才走新逻辑；旧配置继续原策略。身份指纹阻止把新参数套到旧状态。
- 新策略实盘读取实际待领LP手续费，**不会把假设40%写入真实账户净值**。在线paper仍明确标注未计实际LP费/资金费，40%假设只在离线research命令生效。
- Solana链上指令仍经已隔离的官方Meteora SDK桥接；决策、回测、风控、日志、重启状态与Hyperliquid管理在Rust。EVM运行不需要该Node桥接。

```bash
cargo build --release --locked
./target/release/lp-maker-solana --config config/solana.toml research \\
  --data backtests/solana/2026-09-18-six-month \\
  --grid backtests/solana/2026-09-18-six-month/accepted-parameters.json \\
  --apr 0.4 --output /tmp/solana-hourly-replay.json
./target/release/lp-maker-solana --config config/solana-regime.toml verify-research \\
  --data backtests/solana/2026-09-18-six-month \\
  --apr 0.4 --output /tmp/solana-fine-replay.json
```

第一条是固定候选复算；`grid-round-4.json`保存开发期搜索空间，中间轮次也全部保留。报告使用 `accepted-hourly.json` / `accepted-fine.json`；更早的探索输出可能尚未补计临时全额对冲周转费，不应作为最终收益。

Triton One名称已确认；尚需账户完整RPC/gRPC地址。未使用猜测的认证地址，未用用户私钥签名、未广播任何真实交易。实时监听和完整交换流程仍需在提供端点后验证。

来源：[Meteora Data API](https://dlmm.datapi.meteora.ag/swagger-ui/)、[Meteora官方SDK](https://github.com/MeteoraAg/dlmm-sdk)、[Hyperliquid Info API](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/info-endpoint)、[Triton Yellowstone](https://docs.triton.one/project-yellowstone/dragons-mouth-grpc-subscriptions)。公开原始响应哈希见 `manifest.json`，本次报告输入和代码哈希见 `evidence.json`。
'''
(p/'REPORT.md').write_text(text)
try:
 import matplotlib
 matplotlib.use('Agg')
 import matplotlib.pyplot as plt
 import matplotlib.dates as md
 dates=[dt.datetime.fromtimestamp(x['time_ms']/1000,dt.timezone.utc) for x in h['equity_curve']]
 fine_dates=[dt.datetime.fromtimestamp(x['time_ms']/1000,dt.timezone.utc) for x in fine['equity_curve']]
 fig,ax=plt.subplots(3,1,figsize=(12,10),gridspec_kw={'height_ratios':[1,1.2,1]},layout='constrained')
 fig.suptitle('SOL / USDC · Six-month LP strategy research\nHypothetical 40% LP fee APR; reserve SOL price risk excluded',fontsize=15,fontweight='bold')
 ax[0].plot(dates,[x['price'] for x in h['equity_curve']],color='#667085',lw=1)
 for i,x in enumerate(h['equity_curve'][:-1]):
  if x['lp_value']>0:ax[0].axvspan(dates[i],dates[i+1],color='#12b76a',alpha=.22,lw=0)
 ax[0].set_title('Observed pool price · green = LP deployed (hourly model)',loc='left');ax[0].set_ylabel('SOL / USDC')
 ax[1].plot(dates,[x['equity'] for x in h['equity_curve']],label='Observed hourly markets (primary)',lw=1.7,color='#155eef')
 ax[1].plot(fine_dates,[x['equity'] for x in fine['equity_curve']],label='5m pool + mostly proxy hedge (sensitivity)',lw=1.2,ls='--',color='#f79009')
 ax[1].axhline(200,color='#98a2b3',lw=1,ls=':');ax[1].set_ylabel('Model equity / USD');ax[1].legend(loc='upper left',fontsize=9)
 names=['Hourly primary','Hourly: 2x costs','Last 62d: fresh start','5m proxy scenario']
 values=[h['pnl'],j['double_cost']['pnl'],q['pnl'],fine['pnl']]
 ax[2].barh(names,values,color=['#155eef' if v>=0 else '#d92d20' for v in values]);ax[2].axvline(0,color='#98a2b3',lw=1)
 for i,v in enumerate(values):ax[2].text(v+(.07 if v>=0 else -.07),i,f'{v:+.2f}',ha='left' if v>=0 else 'right',va='center',fontsize=10)
 ax[2].set_xlim(min(values)-1,max(values)+1);ax[2].set_xlabel('Net model PnL / USD');ax[2].invert_yaxis()
 for a in ax:
  a.spines[['top','right']].set_visible(False);a.grid(axis='y',alpha=.12)
 for a in ax[:2]:a.xaxis.set_major_formatter(md.DateFormatter('%b %d'))
 fig.savefig(p/'six-month.png',dpi=160,bbox_inches='tight');plt.close(fig)
except ImportError:
 print('Matplotlib unavailable; report and CSV written, chart not generated.')
print(json.dumps({'pnl':h['pnl'],'lp_fees':h['lp_fees'],'rent_sol':rent,'report':str(p/'REPORT.md')},ensure_ascii=False))
