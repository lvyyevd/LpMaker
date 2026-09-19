#!/usr/bin/env python3
"""Public, credential-free six-calendar-month data with auditable raw responses.

Never forward-fill prices or funding. A failed coverage check fails the export.
The fixed window also avoids accidentally optimizing against a moving end date.
"""
import argparse
import concurrent.futures
import datetime as dt
import hashlib
import json
import pathlib
import subprocess
import time

POOL = "5rCf1DM8LjKTw4YqhnoLcngyZYeNnQqztScTogYHAS6"
BASE = "https://dlmm.datapi.meteora.ag"
HL = "https://api.hyperliquid.xyz/info"


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--output", required=True)
    p.add_argument("--fine", action="store_true", help="also fetch full-period pool 5m bars")
    a = p.parse_args()
    out = pathlib.Path(a.output)
    raw = out / "raw"
    raw.mkdir(parents=True, exist_ok=True)
    start = int(dt.datetime(2026, 3, 18, 8, tzinfo=dt.timezone.utc).timestamp())
    end = int(dt.datetime(2026, 9, 18, 8, tzinfo=dt.timezone.utc).timestamp())
    warm = start - 19 * 86400

    def fetch(name, url, payload=None):
        path = raw / (name + ".json")
        if path.exists():
            return json.loads(path.read_text())
        for attempt in range(6):
            args = ["curl", "-fsS", "--max-time", "30", url]
            if payload is not None:
                args += ["-H", "Content-Type: application/json", "--data-binary", json.dumps(payload)]
            r = subprocess.run(args, capture_output=True, text=True)
            try:
                if r.returncode:
                    raise ValueError("HTTP failure")
                value = json.loads(r.stdout)
                if isinstance(value, dict) and ("error" in value or "message" in value):
                    raise ValueError("API error")
                path.write_text(r.stdout)
                return value
            except ValueError:
                if attempt == 5:
                    raise RuntimeError("Public history request failed: " + name)
                time.sleep(min(2 ** attempt, 16))

    jobs = []
    for t in range(start, end, 72 * 3600):
        jobs.append((f"pool_1h_{t}", f"{BASE}/pools/{POOL}/ohlcv?timeframe=1h&start_time={t}&end_time={min(t+72*3600,end)-1}"))
    for t in range(warm, end, 7 * 86400):
        jobs.append((f"hl_1h_{t}", HL, {"type": "candleSnapshot", "req": {"coin": "SOL", "interval": "1h", "startTime": t * 1000, "endTime": min(t + 7 * 86400, end) * 1000 - 1}}))
    if a.fine:
        for t in range(start, end, 6 * 3600):
            jobs.append((f"pool_5m_{t}", f"{BASE}/pools/{POOL}/ohlcv?timeframe=5m&start_time={t}&end_time={min(t+6*3600,end)-1}"))
    with concurrent.futures.ThreadPoolExecutor(max_workers=4) as ex:
        for i, _ in enumerate(ex.map(lambda args: fetch(*args), jobs), 1):
            if i % 25 == 0:
                print(f"Public observations: {i}/{len(jobs)}", flush=True)

    quality = {}

    def combine(prefix, field, lo, step, nested=False):
        unique = {}
        duplicate_count = 0
        for path in sorted(raw.glob(prefix + "*.json")):
            rows = json.loads(path.read_text())
            for row in rows["data"] if nested else rows:
                t = int(row[field])
                if t in unique:
                    duplicate_count += 1
                    if unique[t] != row:
                        raise ValueError(f"Conflicting duplicate: {prefix} {t}")
                unique[t] = row
        scale = 1000 if field == "t" else 1
        expected = set(range(lo * scale, end * scale, step * scale))
        missing = expected - unique.keys()
        extra = unique.keys() - expected
        quality[prefix] = {"rows": len(unique), "missing": len(missing), "extra": len(extra), "duplicates": duplicate_count}
        quality[prefix]["missing_timestamps"] = sorted(missing)
        # A pool can have a no-trade interval; do not synthesize a candle.
        # Fine validation explicitly stops accrual across these gaps.
        if (missing and prefix != "pool_5m_") or extra:
            raise ValueError(f"Coverage failed: {prefix}: {quality[prefix]}")
        result = [unique[t] for t in sorted(unique)]
        for row in result:
            o, h, l, c = [float(row[k]) for k in (("open", "high", "low", "close") if nested else ("o", "h", "l", "c"))]
            if not (0 < l <= min(o, c) <= max(o, c) <= h):
                raise ValueError(f"Invalid OHLC: {prefix}")
        return result

    pool = combine("pool_1h_", "timestamp", start, 3600, True)
    hedge = combine("hl_1h_", "t", warm, 3600)
    funding = []
    cursor = start * 1000
    while cursor < end * 1000:
        rows = fetch(f"funding_{cursor}", HL, {"type": "fundingHistory", "coin": "SOL", "startTime": cursor, "endTime": end * 1000 - 1})
        if not rows:
            break
        funding.extend(rows)
        next_cursor = max(int(x["time"]) for x in rows) + 1
        if next_cursor <= cursor:
            raise ValueError("Funding pagination did not advance")
        cursor = next_cursor
    # Actual settlement timestamps are a few milliseconds after the hour.
    # Bucket only for coverage validation; preserve original times in exported data.
    funding_by_time = {int(x["time"]) // 3600000 * 3600000: x for x in funding}
    expected = set(range(start * 1000, end * 1000, 3600000))
    quality["funding"] = {"rows": len(funding), "missing": len(expected - funding_by_time.keys()), "duplicates": len(funding) - len(funding_by_time)}
    if quality["funding"]["missing"] or quality["funding"]["duplicates"]:
        raise ValueError("Funding coverage failed")
    exports = [("pool_1h", pool), ("hyperliquid_1h", hedge), ("funding", funding)]
    if a.fine:
        exports.append(("pool_5m", combine("pool_5m_", "timestamp", start, 300, True)))
    for name, value in exports:
        (out / (name + ".json")).write_text(json.dumps(value, separators=(",", ":")))
    meta = fetch("pool_metadata", f"{BASE}/pools/{POOL}")
    (out / "metadata.json").write_text(json.dumps(meta, indent=2))
    manifest = {"start_ms": start * 1000, "end_ms": end * 1000, "warmup_ms": warm * 1000,
                "retrieved_utc": dt.datetime.now(dt.timezone.utc).isoformat(), "pool": POOL,
                "sources": [BASE, HL], "quality": quality,
                "fee_assumption": "40% simple APR on eligible in-range principal-time, not measured fees",
                "raw_sha256": {f.name: hashlib.sha256(f.read_bytes()).hexdigest() for f in sorted(raw.glob("*.json"))}}
    (out / "manifest.json").write_text(json.dumps(manifest, indent=2))
    print(json.dumps(quality, indent=2), flush=True)


if __name__ == "__main__":
    main()
