#!/usr/bin/env python3
"""Download public observations only. No wallet or Triton credentials are read.
Keep raw responses and hashes so a report can be audited without repeating HTTP calls.
"""
import argparse, concurrent.futures, datetime as dt, hashlib, json, pathlib, subprocess, time
POOL='5rCf1DM8LjKTw4YqhnoLcngyZYeNnQqztScTogYHAS6'
BASE='https://dlmm.datapi.meteora.ag'
p=argparse.ArgumentParser(); p.add_argument('--end',required=True,help='UTC exclusive end, YYYY-MM-DDTHH:00:00Z');p.add_argument('--output',required=True);a=p.parse_args()
end=int(dt.datetime.fromisoformat(a.end.replace('Z','+00:00')).timestamp());start=end-30*86400
out=pathlib.Path(a.output);raw=out/'raw';raw.mkdir(parents=True,exist_ok=True)
def fetch(name,url,payload=None):
 path=raw/(name+'.json')
 if path.exists():return json.loads(path.read_text())
 for attempt in range(4):
  cmd=['curl','-fsS','--max-time','30',url]
  if payload is not None:cmd+=['-H','Content-Type: application/json','--data-binary',json.dumps(payload)]
  r=subprocess.run(cmd,capture_output=True,text=True)
  try:
   if r.returncode:raise ValueError('HTTP failed')
   j=json.loads(r.stdout)
   if isinstance(j,dict) and ('error' in j or 'message' in j):raise ValueError(str(j))
   path.write_text(r.stdout);return j
  except (ValueError,json.JSONDecodeError):
   if attempt==3:raise RuntimeError('Public data request failed: '+name)
   time.sleep(1+attempt)
meta=fetch('pool',f'{BASE}/pools/{POOL}')
jobs=[]
# Endpoint limits are narrower than the full month; request six hours per slice.
for t in range(start,end,6*3600):
 for kind,route in [('pool','ohlcv'),('fees','volume/history')]:
  jobs.append((f'{kind}_{t}',f'{BASE}/pools/{POOL}/{route}?timeframe=5m&start_time={t}&end_time={min(t+21600,end)-1}',None))
for interval,step,warm in [('5m',3*86400,0),('1h',7*86400,8*86400)]:
 for t in range(start-warm,end,step):
  jobs.append((f'hl_{interval}_{t}','https://api.hyperliquid.xyz/info',{'type':'candleSnapshot','req':{'coin':'SOL','interval':interval,'startTime':t*1000,'endTime':min(t+step,end)*1000-1}}))
jobs.append(('funding','https://api.hyperliquid.xyz/info',{'type':'fundingHistory','coin':'SOL','startTime':start*1000,'endTime':end*1000-1}))
with concurrent.futures.ThreadPoolExecutor(max_workers=4) as ex:
 for i,_ in enumerate(ex.map(lambda v:fetch(*v),jobs),1):
  if i%25==0:print(f'public history: {i}/{len(jobs)}',flush=True)
def combined(prefix,key=None):
 rows=[]
 for f in sorted(raw.glob(prefix+'*.json')):
  j=json.loads(f.read_text());rows.extend(j[key] if key else j)
 field='timestamp' if key else 't'
 unique={int(r[field]):r for r in rows};return [unique[k] for k in sorted(unique)]
pool=combined('pool_','data');fees=combined('fees_','data');hl=combined('hl_5m_');hourly=combined('hl_1h_')
fund=json.loads((raw/'funding.json').read_text())
while fund and len(fund)%500==0 and int(fund[-1]['time'])<end*1000-1:
 cursor=int(fund[-1]['time'])+1
 page=fetch(f'funding_after_{cursor}','https://api.hyperliquid.xyz/info',{'type':'fundingHistory','coin':'SOL','startTime':cursor,'endTime':end*1000-1})
 if not page:break
 fund.extend(page)
fund=list({int(x['time']):x for x in fund}.values());fund.sort(key=lambda x:int(x['time']))
def coverage(rows,field,lo,step):
 actual={int(x[field]) for x in rows};expected=set(range(lo,end*(1000 if field=='t' else 1),step))
 return {'rows':len(rows),'missing':len(expected-actual),'first':min(actual) if actual else None,'last':max(actual) if actual else None}
quality={'pool_5m':coverage(pool,'timestamp',start,300),'hl_5m':coverage(hl,'t',start*1000,300000),'hl_1h':coverage(hourly,'t',(start-8*86400)*1000,3600000),'fee_zero_with_volume':sum(1 for x in fees if x['fees']==0 and x['volume']>0),'fee_rows':len(fees),'fee_missing_vs_ohlcv':sum(1 for x in fees if x['volume']==0 and next((r['volume'] for r in pool if r['timestamp']==x['timestamp']),0)>0),'funding_rows':len(fund)}
for name,rows in [('pool',pool),('fees',fees),('hyperliquid_5m',hl),('hyperliquid_1h',hourly),('funding',fund)]:
 (out/(name+'.json')).write_text(json.dumps(rows,separators=(',',':')))
manifest={'pool':POOL,'start_ms':start*1000,'end_ms':end*1000,'end_exclusive_utc':a.end,'retrieved_utc':dt.datetime.now(dt.timezone.utc).isoformat(),'sources':[BASE,'https://api.hyperliquid.xyz/info'],'quality':quality,'missing_data':'No historical per-bin liquidity ownership or maker order queue available; personal LP fees and maker fills cannot be measured exactly.','raw_sha256':{f.name:hashlib.sha256(f.read_bytes()).hexdigest() for f in sorted(raw.glob('*.json'))}}
(out/'manifest.json').write_text(json.dumps(manifest,indent=2));print(json.dumps(quality,indent=2))

# Current rent quote is used only to filter initial capital feasibility, never as historical PnL.
rpc='https://solana-rpc.publicnode.com'
def rent(size):return fetch(f'rent_{size}',rpc,{'jsonrpc':'2.0','id':1,'method':'getMinimumBalanceForRentExemption','params':[size]})['result']
base_rent=rent(8112);extra_rent=rent(8224)-base_rent
(out/'rent.json').write_text(json.dumps({'observed_utc':dt.datetime.now(dt.timezone.utc).isoformat(),'source':'Solana mainnet getMinimumBalanceForRentExemption, SDK 1.9.14 position size','base_position_bytes':8112,'base_position_bins':70,'extra_bin_bytes':112,'base_position_lamports':base_rent,'extra_bin_lamports':extra_rent,'assumption':'Current schedule for initial feasibility, not historical rent PnL. Missing bin-array rent excluded; live simulation checks it.'},indent=2))

manifest['raw_sha256']={f.name:hashlib.sha256(f.read_bytes()).hexdigest() for f in sorted(raw.glob('*.json'))}
(out/'manifest.json').write_text(json.dumps(manifest,indent=2))
