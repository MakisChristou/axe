#!/usr/bin/env python3
"""Measure the Solana gateway message-approval CU from real mainnet transactions.

The approval flow (secp256k1 verifier-set verification) is what Niko's ~3.1M
figure refers to. It is split across many transactions because a single Solana
tx is capped at 1.4M CU, so it cannot be measured in one LiteSVM transaction.
Instead we read `computeUnitsConsumed` from recent mainnet gateway transactions
and reconstruct one full approval batch.

    python3 gateway_approval_cu.py

No API key needed; uses a public RPC (send a browser User-Agent or it 403s).
"""
import json, urllib.request, time, collections, re

RPC = "https://solana-rpc.publicnode.com"
GW = "gtwqvLL93XK7pC2eMvfGamqokvs19AytzaVhrL2iKiz"  # mainnet gateway program id
IX = re.compile(r"Program log: Instruction: (\w+)")

def rpc(method, params):
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
    for _ in range(3):
        try:
            req = urllib.request.Request(
                RPC, data=body,
                headers={"Content-Type": "application/json", "User-Agent": "Mozilla/5.0"})
            r = json.load(urllib.request.urlopen(req, timeout=15))
            if r.get("result") is not None:
                return r["result"]
        except Exception:
            pass
        time.sleep(0.6)
    return None

# 1. page back through recent gateway signatures
sigs, before = [], None
for _ in range(6):
    p = [GW, {"limit": 100} if before is None else {"limit": 100, "before": before}]
    batch = rpc("getSignaturesForAddress", p)
    if not batch:
        break
    sigs += batch
    before = batch[-1]["signature"]
print("signatures scanned:", len(sigs))

# 2. classify each tx by its top-level gateway instruction and read its CU
by = collections.defaultdict(list)
recs = []
deadline = time.time() + 170
for s in sigs:
    if time.time() > deadline:
        break
    if s.get("err"):
        continue
    tx = rpc("getTransaction", [s["signature"], {"maxSupportedTransactionVersion": 0}])
    if not tx:
        continue
    meta = tx.get("meta") or {}
    cu, logs = meta.get("computeUnitsConsumed"), meta.get("logMessages") or []
    top = None
    for i, l in enumerate(logs):
        if l.startswith(f"Program {GW} invoke [1]"):
            for l2 in logs[i + 1:i + 4]:
                m = IX.search(l2)
                if m:
                    top = m.group(1)
                    break
            break
    if cu is not None:
        by[top or "?"].append(cu)
        recs.append({"name": top or "?", "cu": cu, "slot": s.get("slot")})

med = lambda xs: sorted(xs)[len(xs) // 2]
print("\n=== per-instruction CU (mainnet) ===")
for name in sorted(by, key=lambda k: -med(by[k])):
    xs = by[name]
    print(f"  {name:<36} n={len(xs):<3} min={min(xs)} median={med(xs)} max={max(xs)}")

# 3. reconstruct init-delimited approval batches (bursts of consecutive slots)
APPROVAL = {"InitializePayloadVerificationSession", "VerifySignature", "ApproveMessage", "ApproveMessages"}
appr = sorted((r for r in recs if r["name"] in APPROVAL), key=lambda r: r.get("slot") or 0)
batches, cur = [], None
for r in appr:
    if r["name"] == "InitializePayloadVerificationSession":
        cur = {"init": r["cu"], "verify": [], "approve": [], "slot": r.get("slot")}
        batches.append(cur)
    elif cur is not None:
        (cur["verify"] if r["name"] == "VerifySignature" else cur["approve"]).append(r["cu"])

print("\n=== reconstructed approval batches ===")
totals = []
for b in batches:
    if len(b["verify"]) >= 8 and b["approve"]:  # complete within the scan window
        tot = b["init"] + sum(b["verify"]) + sum(b["approve"])
        totals.append(tot)
        print(f"  slot={b['slot']}  init={b['init']}  {len(b['verify'])}x verify(Σ={sum(b['verify'])})  "
              f"{len(b['approve'])}x approve(Σ={sum(b['approve'])})  TOTAL={tot}")
if totals:
    print(f"\nbatch total CU: min={min(totals):,} median={sorted(totals)[len(totals)//2]:,} max={max(totals):,}")
