#!/usr/bin/env python3
"""Read the *charged* compute-unit budgets from real mainnet ITS transactions.

Solana prices the priority fee on the compute-unit limit a transaction requests
and never refunds the remainder, so what the fee-api must quote is the limit, not
`compute_units_consumed`. The LiteSVM harness next door measures consumption
against the real bytecode but with minimal injected state, and it lands 20-25%
under real mainnet. This script is the calibration source: it reads the limits
actual transactions carry, per operation variant, so `[cost.solana]` can be read
straight off the output.

    python3 mainnet_cu_limits.py

For each transaction the charged budget is either the explicit
`SetComputeUnitLimit`, or - when there is none - Solana's default allocation of
200k CU per instruction, capped at 1.4M.

Variants are separated because they cost materially different amounts:

  * `ata+`     the ITS `init_if_needed` on the recipient ATA fired, so `execute`
               also created the account. A quote cannot know which branch runs,
               so this is the one to budget for.
  * `withcall` `execute` went on to CPI a destination program
               (`ExecuteWithInterchainToken`), i.e. an itsTransferWithCall.

No API key needed; uses a public RPC (send a browser User-Agent or it 403s).
Override it with SOLANA_RPC_URL - the public endpoints are slow enough that the
rarer variants (`ata+`, `withcall`) may not appear in a default run.
"""
import collections
import json
import os
import re
import struct
import time
import urllib.request

RPC = os.environ.get("SOLANA_RPC_URL", "https://solana-rpc.publicnode.com")
ITS = "itsAUdHnV2K99ppbM6d6WUDac8MD54RAE7dUKHnw1Eg"  # mainnet ITS program id
GW = "gtwqvLL93XK7pC2eMvfGamqokvs19AytzaVhrL2iKiz"  # mainnet gateway program id
CB = "ComputeBudget111111111111111111111111111111"
IX = re.compile(r"Program log: Instruction: (\w+)")
B58 = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"

# ComputeBudget instruction tags.
SET_LIMIT, SET_PRICE = 2, 3
# Solana's per-instruction default when a transaction sets no explicit limit.
DEFAULT_CU_PER_IX, MAX_TX_CU = 200_000, 1_400_000
# How long to spend pulling transactions before reporting what we have.
BUDGET_SECONDS = 240


def rpc(method, params):
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
    for _ in range(3):
        try:
            req = urllib.request.Request(
                RPC,
                data=body,
                headers={"Content-Type": "application/json", "User-Agent": "Mozilla/5.0"},
            )
            r = json.load(urllib.request.urlopen(req, timeout=20))
            if r.get("result") is not None:
                return r["result"]
        except Exception:
            pass
        time.sleep(0.6)
    return None


def b58_decode(data):
    n = 0
    for c in data.encode():
        n = n * 58 + B58.index(c)
    return n.to_bytes((n.bit_length() + 7) // 8, "big")


def compute_budget(instructions):
    """The explicit (limit, price) a transaction's ComputeBudget instructions set."""
    limit = price = None
    for ix in instructions:
        if ix.get("programId") != CB:
            continue
        raw = b58_decode(ix.get("data", ""))
        if not raw:
            continue
        if raw[0] == SET_LIMIT and len(raw) >= 5:
            limit = struct.unpack("<I", raw[1:5])[0]
        elif raw[0] == SET_PRICE and len(raw) >= 9:
            price = struct.unpack("<Q", raw[1:9])[0]
    return limit, price


def variant(name, logs):
    """Split an instruction into the variants that cost materially different CU."""
    tags = []
    if any("Instruction: ExecuteWithInterchainToken" in l for l in logs):
        tags.append("withcall")
    if any(l == "Program log: Create" for l in logs):
        tags.append("ata+")
    return f"{name} / {'+'.join(tags) if tags else 'plain'}"


def scan(program, limit_pages=6, only=None):
    """Charged/consumed CU per instruction variant for one program's transactions.

    `only` keeps just the named instructions. An ITS transaction CPIs the gateway
    and so appears in both programs' signature lists; without it the gateway
    report restates the ITS rows.
    """
    sigs, before = [], None
    for _ in range(limit_pages):
        params = [program, {"limit": 100} if before is None else {"limit": 100, "before": before}]
        batch = rpc("getSignaturesForAddress", params)
        if not batch:
            break
        sigs += batch
        before = batch[-1]["signature"]

    rows = collections.defaultdict(list)
    deadline = time.time() + BUDGET_SECONDS
    scanned = 0
    for s in sigs:
        if time.time() > deadline:
            break
        if s.get("err"):
            continue
        tx = rpc(
            "getTransaction",
            [s["signature"], {"maxSupportedTransactionVersion": 0, "encoding": "jsonParsed"}],
        )
        if not tx:
            continue
        scanned += 1
        meta, msg = tx["meta"], tx["transaction"]["message"]
        logs = meta.get("logMessages") or []
        match = next((IX.search(l) for l in logs if IX.search(l)), None)
        if not match or (only and match.group(1) not in only):
            continue
        instructions = msg["instructions"]
        explicit, price = compute_budget(instructions)
        charged = (
            explicit
            if explicit is not None
            else min(DEFAULT_CU_PER_IX * len(instructions), MAX_TX_CU)
        )
        rows[variant(match.group(1), logs)].append(
            {
                "charged": charged,
                "consumed": meta.get("computeUnitsConsumed"),
                "explicit": explicit is not None,
                "pays_priority": bool(price),
            }
        )
    return scanned, rows


def report(title, scanned, rows):
    print(f"\n=== {title} (scanned {scanned} transactions) ===")
    header = f"{'variant':<34} {'n':>4} {'consumed med':>13} {'CHARGED med':>12} {'CHARGED max':>12} {'no limit set':>13} {'pays priority':>14}"
    print(header)
    print("-" * len(header))
    for name, entries in sorted(rows.items()):
        consumed = sorted(e["consumed"] for e in entries if e["consumed"])
        charged = sorted(e["charged"] for e in entries)
        if not consumed:
            continue
        implicit = sum(1 for e in entries if not e["explicit"])
        paying = sum(1 for e in entries if e["pays_priority"])
        print(
            f"{name:<34} {len(entries):>4} {consumed[len(consumed) // 2]:>13,} "
            f"{charged[len(charged) // 2]:>12,} {charged[-1]:>12,} "
            f"{implicit:>13} {paying:>14}"
        )


APPROVAL = {"InitializePayloadVerificationSession", "VerifySignature", "ApproveMessage"}

its_scanned, its_rows = scan(ITS)
report("ITS program", its_scanned, its_rows)

gw_scanned, gw_rows = scan(GW, limit_pages=3, only=APPROVAL)
report("Gateway approval workflow", gw_scanned, gw_rows)

print(
    """
Read `[cost.solana]` off the CHARGED max column:

  execution_compute_units.itsTransfer          Execute / ata+
  execution_compute_units.itsTransferWithCall  Execute / withcall+ata+
  source_compute_units.itsTransfer             InterchainTransfer / *
  approve_compute_units                        the largest row in the approval
                                               workflow (VerifySignature)
"""
)

missing = [v for v in ("Execute / ata+", "Execute / withcall+ata+") if v not in its_rows]
if missing:
    print(
        "NOT SAMPLED in this run: "
        + ", ".join(missing)
        + "\nThese are the rarer variants and the ones the transfer budgets are read from."
        "\nRe-run against a faster endpoint (SOLANA_RPC_URL) before trusting the table."
    )
