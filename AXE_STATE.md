# AXE — State of cross-chain test transfers

_Last updated: 2026-08-25. Branch: `fix/cron-xrpl-transients`._

`axe test load-test` drives real cross-chain transfers (GMP `callContract` and
ITS `interchainTransfer`) and verifies them **on-chain**. A route is "✅
executed" only when the destination message reached `executed`
(`isCommandExecuted` / dest-app execution / `MessageExecuted`), not merely
`approved`.

This document is the validated state of what works, what is still open, and the
**known limitations** we need to tackle. It is grounded in three sources of
truth, in priority order:

1. The dispatch match in `src/commands/load_test.rs` — defines the
   **supported route surface** (which `(protocol, test-type)` pairs run vs. bail).
2. The wired chain set in `.github/actions/run-loadtest/action.yml` `CHAIN_MAP`
   (28 chains) — the chains the harness can target.
3. The on-chain results in `axe-load-test-logs/*.json` — the **validated** truth.

Companion docs: per-pair dispatcher matrices in
[`docs/routes.md`](docs/routes.md) and chain-type coverage in
[`docs/load-test-coverage.md`](docs/load-test-coverage.md). Both are
reconciled with the 28-chain `CHAIN_MAP` (2026-08-25).

---

## 1. The one fact that frames everything

**On mainnet, every EVM chain is a legacy/consensus chain** — none has an
Amplifier `VotingVerifier`. So on mainnet:

- **EVM ↔ EVM = legacy ↔ legacy** (Amplifier-EVM does not exist on mainnet).
- **Amplifier** on mainnet means only the **non-EVM** chains: Solana, Sui,
  Stellar, XRPL.

Amplifier-EVM chains (with a `VotingVerifier`) exist only on **testnet**
(e.g. `monad-3`, `celo-sepolia`, `xrpl-evm`, `hyperliquid`). This is why
legacy-chain support is load-bearing for mainnet.

---

## 2. Supported route surface (from the dispatcher)

The 28 wired chains (`CHAIN_MAP`): Arbitrum, Arc, Avalanche, Base, Berachain,
Binance, Blast, Celo, Ethereum, Filecoin, Flow, Hedera, Hyperliquid, Immutable,
Kava, Linea, Mantle, Monad, Moonbeam, Optimism, Plume, Polygon, Scroll, Solana,
Stellar, Sui, XRPL, XRPL EVM. Arc/Berachain/Celo/Flow/Plume exist only on
stagenet / devnet-amplifier — see [`docs/routes.md`](docs/routes.md) for the
per-network roster. Purged (gone from the repo): Fantom, Centrifuge.

Chain-type level, what the dispatcher runs (✅) vs. bails on (see §6 for why):

### GMP (`callContract`)

| src ↓ \ dst → | EVM | Solana | Stellar | Sui | XRPL |
|---|---|---|---|---|---|
| **EVM**     | ✅ | ✅ | ✅ | ✅ | ✖ by design |
| **Solana**  | ✅ | ✅ | ✅ | ✅ | ✖ by design |
| **Stellar** | ✅ | ✅ | — | ✅ | ✖ by design |
| **Sui**     | ✅ | ✅ | ⛔ not built | — | ✖ by design |
| **XRPL**    | ✖ by design | ✖ | ✖ | ✖ | — |

### ITS (`interchainTransfer` via the hub)

| src ↓ \ dst → | EVM | Solana | Stellar | Sui | XRPL |
|---|---|---|---|---|---|
| **EVM**     | ✅ | ✅ | ✅ | ✅ | ✅ canonical XRP |
| **Solana**  | ✅ | ⛔ not built | ⛔ not built | ✅ | ⛔ not built |
| **Stellar** | ✅ | ⚠️ untrusted (§6.2) | — | ✅ | ⛔ not built |
| **Sui**     | ✅ | ✅ | ⛔ not built | — | ⛔ not built |
| **XRPL**    | ✅ canonical XRP | ⛔ not built | ⛔ not built | ⛔ not built | — |

`its-with-data` (transfer + dest contract call) is wired for `evm → sol` only.

GMP is the cross-chain **delivery** primitive; ITS rides the identical
verify→route→approve→execute path and additionally needs its token registered on
each endpoint. Validating GMP validates the delivery path for both.

### `test express-execution` — express-reimbursement monitor (observe-only)

`axe test express-execution <chains…> [--source-tx <hash>] [--network …]
[--recent N] [--timeout-secs N]` monitors Axelar **express execution
reimbursement** via the Axelarscan GMP API (`/gmp/searchGMP`; testnet base for
testnet/stagenet/devnet, mainnet base for mainnet). Express = a relayer fronts
tokens to the recipient on the destination ITS edge (`expressExecute`) *before*
the canonical GMP proof lands, then is **reimbursed** when the canonical
`ITS.execute` lands (`ExpressExecutionFulfilled` fires atomically inside that
execute tx). The command reports two phases per transfer: **Phase 1** —
express executed (executor EOA / contract + express tx), and **Phase 2** —
executor reimbursed (canonical execute tx), or PENDING/timeout if the execute
hasn't landed. On reimbursement it also runs an **amount check** (MOU-27): it
decodes the executor EOA's outbound ERC-20 `Transfer`s in the express tx
(fronted) and its inbound `Transfer`s in the execute tx (reimbursed), both from
the GMP-API receipt logs, and asserts the two are equal. A mismatch or a missing
inbound transfer is surfaced as an error and **fails** the single-tx watch
(non-zero exit); the fronted/reimbursed base-unit amounts are printed either way.
Two modes: a chains scan (newest `--recent` express transfers per chain) and a
single-tx watch (`--source-tx`, polled every 10 s up to `--timeout-secs`, default
1800). It needs no wallet keys, RPCs, or chains-config — GMP-API reads only. CI:
`.github/workflows/test-express-execution.yml`.

**v1 is monitor-only.** axe does **not** yet originate a qualifying express
transfer — producing one requires routing through an express-enabled project
(e.g. the Squid router), which is an open board decision tracked as a follow-up.
v1 only observes reimbursement on transfers initiated elsewhere.

**On-chain reimbursement verified (MOU-26, mainnet, 2026-06-25).** The monitor's
API-derived "reimbursed" flag was cross-checked against the destination-chain
receipts for three real express transfers spanning the finality spectrum. In
every case the express executor EOA `0xe743…cea84` pays out token amount X in the
express tx and receives the **exact same X** back (mint → executor) inside the
canonical execute tx — full, exact-amount reimbursement, not just "execute
landed":

| source (finality) | route | express→execute gap | amount fronted = reimbursed |
| --- | --- | --- | --- |
| avalanche (~instant) | → moonbeam | 78 s | `0xbe4488` |
| base (moderate) | → avalanche | 1527 s | `0x119db3af5` |
| ethereum (~13–16 min) | → polygon | 1079 s | `0x49292` |

Two ethereum-source transfers were also caught mid-flight (`express_executed`,
execute not yet landed) — the source-finality wait the monitor reports as Phase 2
PENDING. The amount-equality invariant above is now asserted **by the tool**
(MOU-27): Phase 2 decodes the executor EOA's outbound `Transfer` in the express
tx and inbound `Transfer` in the execute tx from the GMP-API receipt logs and
fails on mismatch or missing inbound — no longer a manual check.

---

## 3. Validated on-chain — executed end-to-end

Latest log per `(src, dst, protocol)`; a route appears here only when the
destination message reached `executed`. Chain ids carry the network
(`*-sepolia`/`*-2`/`*-3`/`*-q1` = testnet).

### 3a. Mainnet — legacy EVM ↔ legacy EVM (GMP)

avalanche → {base, binance, blast, ethereum, kava, moonbeam, polygon, scroll};
binance → {avalanche, fraxtal, immutable, linea}; immutable → binance;
kava → {avalanche, filecoin, moonbeam}; moonbeam → kava.

Chains exercised as **source**: avalanche, binance, immutable, kava, moonbeam.
Chains validated as **destination** (executed): avalanche, base, binance, blast,
ethereum, filecoin, fraxtal, immutable, kava, linea, moonbeam, polygon, scroll.
`kava → filecoin` confirms filecoin **executes on mainnet** (its testnet relayer
does not).

### 3b. Mainnet — non-EVM amplifier + legacy↔non-EVM (GMP & ITS)

| Route | Class | Proto |
|---|---|---|
| avalanche → solana | legacy-EVM → amplifier non-EVM | GMP ✅ |
| avalanche → stellar | legacy-EVM → amplifier non-EVM | GMP ✅ |
| avalanche → sui | legacy-EVM → amplifier non-EVM | GMP ✅ |
| stellar → arbitrum | amplifier non-EVM → legacy-EVM | GMP ✅ |
| stellar → solana | amplifier non-EVM ↔ non-EVM | GMP ✅ |
| solana → sui | amplifier non-EVM ↔ non-EVM | GMP ✅ + ITS ✅ |
| sui → {avalanche, hyperliquid} | non-EVM → EVM | GMP ✅ |
| sui → solana | non-EVM ↔ non-EVM | ITS ✅ |
| hedera → sui | EVM → non-EVM | GMP ✅ |
| hyperliquid → {hedera, stellar} | EVM → mixed | GMP ✅ |
| hyperliquid → stellar | EVM → non-EVM | ITS ✅ |
| xrpl ↔ xrpl-evm | XRPL canonical XRP | ITS ✅ both ways |
| xrpl-evm → {avalanche, solana} | amplifier-EVM → mixed | ITS ✅ |

### 3c. Testnet — amplifier-EVM pipeline (full voted→routed→approved→executed)

| Route | Class | Proto |
|---|---|---|
| avalanche → celo-sepolia | legacy-EVM → amplifier-EVM | GMP ✅ |
| avalanche → ethereum-sepolia | legacy-EVM → amplifier-EVM | GMP ✅ + ITS ✅ |
| avalanche → {mantle-sepolia, polygon-sepolia} | legacy → amplifier-EVM | GMP ✅ |
| avalanche → stellar-2026-q1-2 | legacy-EVM → amplifier non-EVM | ITS ✅ |
| hyperliquid → celo-sepolia | amplifier-EVM ↔ amplifier-EVM | GMP ✅ |
| hyperliquid → stellar-2026-q1-2 | amplifier-EVM → non-EVM | GMP ✅ + ITS ✅ |
| xrpl-evm → scroll | amplifier-EVM → legacy-EVM | GMP ✅ |
| xrpl-evm → ethereum-sepolia | amplifier-EVM ↔ amplifier-EVM | ITS ✅ |
| solana → stellar-2026-q1-2 | non-EVM → non-EVM | GMP ✅ |
| stellar-2026-q1-2 → {solana, hyperliquid} | non-EVM → mixed | GMP/ITS ✅ |
| monad-3 → {avalanche, ethereum-sepolia, hedera} | amplifier-EVM → mixed | GMP/ITS ✅ |
| hedera → monad-3 | EVM → amplifier-EVM | ITS ✅ |
| linea-sepolia → immutable | amplifier-EVM → EVM | GMP ✅ |

### 3d. Mainnet — MOU-29 15-route validation batch (2026-06-23 – 2026-06-26)

Deliberate end-to-end validation covering amplifier non-EVM ↔ non-EVM, amplifier
EVM ↔ non-EVM, legacy ↔ amplifier non-EVM, and XRPL paths.  A route is ✅ only
when the destination message reached `executed` (GMP-API `executed` state +
destination execute tx confirmed by axe verifier or GMP-API `recovered_via_api`
backstop).

| # | Route | Proto | Class | Source Tx | Result |
|---|---|---|---|---|---|
| 01 | solana → sui | ITS | amplifier non-EVM ↔ non-EVM | `51TcpvU19G9UD97TPFjg8AeQkkLHcRNuKHbqa7W83W` | ✅ executed |
| 02 | sui → solana | ITS | amplifier non-EVM ↔ non-EVM | `wDMmNf5AJsRCAmqQEmoUgHKoJVPa945PameRgAmSd2` | ✅ executed |
| 03 | solana → sui | GMP | amplifier non-EVM ↔ non-EVM | `3UArKocnjH1tbhnhK3Sx8GsW95xsB89m4nVz72hMJ8` | ✅ executed |
| 04 | sui → hyperliquid | GMP | amplifier non-EVM → amplifier EVM | `9xnicQn9UccGRU8V4vgLvotpRGuienfQayoMrqtEfM` | ✅ executed |
| 05 | stellar → solana | GMP | amplifier non-EVM ↔ non-EVM | `0xd8a924923a8868879318ad9eb912da89f8fbc00e` | ✅ executed |
| 06 | xrpl → xrpl-evm | ITS | XRPL canonical XRP | `0x5037290b3a9ec3eccff49fe714803f381c44cd35` | ✅ executed |
| 07 | xrpl-evm → xrpl | ITS | XRPL canonical XRP | `0xbbab736ab1cbc9911b7e22bed8aa6fb37d1bcb55` | ✅ executed |
| 08 | hyperliquid → stellar | ITS | amplifier EVM → non-EVM | `0x1c6c898da42d3edf6a1b991dc27802e7ead44a9c` | ✅ executed (52.5 s) |
| 09 | avalanche → solana | GMP | legacy EVM → amplifier non-EVM | `0x6519283767a8504868a06a82eae632130fcf1c0e` | ✅ executed |
| 10 | avalanche → stellar | GMP | legacy EVM → amplifier non-EVM | `0x6488a701a7ebd008dcfb6d10be5ffd4b850c1cbb` | ✅ executed |
| 11 | avalanche → sui | GMP | legacy EVM → amplifier non-EVM | `0x48abdda2c0b882813c78da72d8719b7b82d127b7` | ✅ executed |
| 12 | stellar → arbitrum | GMP | amplifier non-EVM → legacy EVM | `0x9824e77f21919c58ff6d16792b95680ac4c75020` | ✅ executed |
| 13 | sui → avalanche | GMP | amplifier non-EVM → legacy EVM | `FnJCBmjKRvwAVuYR729NPPSq6usz2H86E4Ph4R3RLu` | ✅ executed |
| 14 | avalanche → base | GMP | legacy EVM ↔ legacy EVM | `0xae0d52ca1de181624df9d75c0fb5b901afa41822` | ✅ executed |
| 15 | kava → moonbeam | GMP | legacy EVM ↔ legacy EVM | `0xbedaf89d3a9be09fa77a7c1425e6af6079b6fe80` | ✅ executed |

**Result: 15/15 ✅**  All routes reached `executed` on destination.

**Part-B reliability findings (MOU-29 audit):**
- Fast non-EVM routes (stellar, sui, solana) typically execute in < 60 s; axe's
  5 s poll interval + live verifier may miss execution before a short per-run cap,
  but the `recovered_via_api` GMP-API backstop correctly catches them.
- EVM destination view-call retry fixed (`8e1f972`) — transient RPC errors on
  `isMessageApproved` / `isMessageExecuted` / `isCommandExecuted` no longer fail
  the verifier loop prematurely.
- **Stellar destination view-call retry fixed (`55e3dbe`)** — same bug class as
  the EVM fix, surfaced empirically by this batch: the hyperliquid→stellar route
  (08) **executed on-chain at T+13–31 s**, but axe's verifier crashed (exit 1) on
  a transient Stellar RPC *connection reset* at `stellar/rpc.rs:746`
  (`simulate_view` → `simulate_transaction_envelope`, no retry). Wrapped the
  read-only simulation in `retry_all`, mirroring the EVM + XRPL patterns.
- `INACTIVITY_TIMEOUT=7200 s` and `POLL_INTERVAL=5 s` are correctly sized for the
  observed latency distribution (max seen: 3226 s for mantle→kava).
- Re-run notes: route 10 (avalanche→stellar) was a source-side setup cap on the
  first pass (SenderReceiver deploy still confirming when the batch cap fired);
  it passes cleanly with a longer cap. Route 14 (avalanche→base) executed on-chain
  at T+2 s; an axe-side verify error only appeared when a token-gated public Base
  RPC rejected the archive `getLogs` scan — an RPC-selection issue, not an axe
  bug (use a private/full Base RPC for the legacy `ContractCallApproved` scan).

---

## 4. Open — routes still needing a run

These are routes from the previous §4 triage list that could not be fully
resolved from log data alone and need a clean re-run.

| Route | Proto | State | Notes |
|---|---|---|---|
| sui → hyperliquid | its (mainnet) | hub_approved T+23 s, no second-leg in GMP-API | Likely CANNOT_EXECUTE (token not on hyperliquid for Sui ITS); re-run to confirm |
| hyperliquid → solana | its (mainnet) | hub_approved T+24 s, no second-leg in GMP-API | Same pattern; re-run to confirm |
| base-sepolia → moonbeam | gmp | NOT run — source wallet had 0 funds | Re-run with funded wallet |

All other former §4 routes are resolved: executed-late and fast-but-missed ones
were verifier bugs, now **fixed on this branch** (§5 items 5–6); real
limitations have been added to §6.

### Resolved §4 triage — executed-late (verifier buffer bugs)

These routes **did execute on-chain** but after the verifier's 1000 s inactivity
window. Measured latencies confirm the buffer must be raised (see §5):

base→polygon (1629 s), celo→optimism (1304 s), filecoin→immutable (1062 s),
fraxtal→mantle (1705 s), mantle→kava (3226 s), mantle→linea (2953 s),
celo→solana (1381 s), arbitrum-sepolia→filecoin-2 (1206 s),
arbitrum-sepolia→moonbeam (1170 s), base-sepolia→filecoin-2 (1642 s),
base-sepolia→scroll (1489 s).

### Resolved §4 triage — polling bugs (executed fast, verifier missed)

These routes executed well within the 1000 s window but the verifier failed to
detect the on-chain state change (destination chain polling broken — see §5):

avalanche→monad-3 gmp (T+31 s), avalanche→monad-3 its (T+18–24 s),
stellar→hyperliquid its (T+37 s total; second-leg T+16 s after routing).

---

## 5. Code fixes already on this branch

1. **Windowed `getLogs`** (`verify/legacy.rs`) — scans the dest gateway in
   100-block windows newest→oldest. Unblocks RPCs that cap the block range
   (polygon Amoy ~128 blocks).
2. **Legacy-gas detection** (`load_test/gas_mode.rs`, `EvmFeeMode`) — chains with
   no EIP-1559 (`baseFeePerGas == null`, e.g. Kava) get type-0 txs with explicit
   `gas_price`. Block is read as raw JSON so non-standard blocks (Moonbeam omits
   `mixHash`) don't break detection.
3. **Paris-bytecode fallback** (`helpers.rs`) — pre-Shanghai chains reject the
   default contract's `PUSH0`; axe probes with `eth_call` and deploys a
   PUSH0-free paris build only when needed.
4. **Single-tx main-wallet send** — `num_txs=1` sends from the main wallet (no
   subwallet funding / parked refund), unblocking thin-balance L2 sources.

### Protocol Engineer fixes (MOU-4) — done on this branch

5. **✅ Raised `INACTIVITY_TIMEOUT`** 1000 s → 7200 s (now in `src/commands/load_test/verification/engine.rs`). The 11
   executed-late routes were cut off by the 1000 s global inactivity window;
   max measured latency was 3226 s (mantle→kava), so 7200 s gives ~2.2x
   headroom. Commit `d558753`.
6. **✅ Fixed amplifier-EVM approve→execute race** (`verify/pipeline.rs`,
   `verify/checks/evm.rs`, `evm.rs`). Root cause was *not* stale RPC data: the
   verifier inferred execution from `isMessageApproved` flipping back to false,
   but a fast route (avalanche→monad-3 gmp T+31 s, its T+18–24 s;
   stellar→hyperliquid its second-leg T+16 s) is approved **and** executed
   between two 5 s polls and never reads `approved == true`, so the tx stuck in
   `Approved` until the inactivity timeout. Added `isMessageExecuted` (which
   stays true after execution) to the gateway ABI and an authoritative
   `Approved if executed` fast-path in both the GMP and ITS-hub amplifier-EVM
   poll branches, mirroring the existing Sui checker. Cannot false-positive
   (it is false until the message genuinely lands). Commit `d558753`.

   Both fixes verified against repo gates (fmt, clippy `-D warnings`, 81 tests).
   On-chain green re-validation of the affected routes is delegated to the CI
   route fleet / QA re-run (see §8).

### Reliability fixes (2026-08, branch `fix/cron-xrpl-transients`)

7. **Cosmos-EVM re-broadcast no longer fatal** (`evm.rs`): XRPL EVM answers a
   same-bytes re-broadcast with `tx already in mempool`; that now falls through
   to receipt-waiting like geth's `already known` instead of failing the route
   (observed killing `xrpl-evm → xrpl` in cron run 32638549694).
8. **All outbound HTTP has bounded timeouts** (`src/http.rs`): one shared
   client (30 s request / 10 s connect) replaces every untimed
   `reqwest::Client::new()` / `reqwest::get`; `clippy.toml` disallows the
   untimed constructors so it cannot regress. Fixes the 30-min silent hang
   that timed out `xrpl → xrpl-evm` in the same cron run.
9. **One repo-wide retry policy**: every backoff ladder (central `retry.rs`,
   EVM nonce contention, the 429 scheduler, Solana confirmed-tx fetch, Stellar
   TxBadSeq, `get_code_with_retry`) is 1 original try + 5 retries at
   4/8/16/32/64 s, +/-20% jitter.
10. **Sui mainnet RPC fallback fixed**: PublicNode retired
    `sui-mainnet-rpc.publicnode.com` (permanent 404, killed preflight in cron
    run 32793045257); the fallback list now uses `sui-rpc.publicnode.com`.

---

## 6. Known limitations (the things to tackle)

These are real, characterized limits — design/protocol/trust/upstream — not
timeouts. Each is something we either accept or drive a fix for.

1. **XRPL has no executable layer (by design).** GMP is impossible in any
   direction touching XRPL. ITS is wired only against EVM endpoints via the
   canonical XRP wrapper (`xrpl ↔ xrpl-evm` validated). No XRPL→XRPL.
2. **ITS trusted-chain gaps.** `stellar → solana` ITS reverts with
   `Contract Error #7 (UntrustedChain)` — the Stellar testnet ITS contract has
   not added `solana` to its trusted-chains list. **Fix is upstream**: the
   contract owner runs `stellar/its.js add-trusted-chains solana` from
   `axelar-contract-deployments`. No axe code change needed. Other endpoint
   pairs may have analogous untrusted-chain gaps — surfaced as a clear revert,
   not a timeout.
3. **Unimplemented dispatcher arms** (bail with an explanatory message, by
   design until built): ITS `sol → stellar`, ITS `sol ↔ sol`, ITS `sui →
   stellar`, ITS `sui → xrpl`, GMP `sui → stellar`, GMP `sui → xrpl`,
   `xrpl → sui` (both protocols). `its-with-data` is `evm → sol` only.
4. **Sui-source GMP voter coverage (upstream).** `Example::gmp::send_call`
   messages from Sui's `GmpChannelId` are rarely voted by the testnet verifier
   set (~130-day p99 historically). The Sui source side is code-complete and
   lands on Axelar, but does not progress past "voted" until verifier coverage
   improves. ITS messages from Sui (`InterchainTokenService.ChannelId`) complete
   in ~20s and are unaffected.
5. **Optimism as a *source* on a fresh deploy** fails with "intrinsic gas too
   high" (an op-stack initcode quirk, every RPC). Works as a **destination** and
   as a source with a cached `SenderReceiver`. Pre-existing, unrelated to the
   legacy work.
6. **Hedera ITS deploy blocker (upstream).** Hedera is excluded from some
   amplifier route cycles pending an upstream Hedera ITS deploy fix (see the
   `TODO(hedera)` in `scripts/test_amplifier_routes.sh`). Hedera works as a
   GMP/ITS endpoint where the token is already deployed (e.g. `monad-3 → hedera`
   ITS ✅, `hedera → sui` GMP ✅).
7. **ITS requires the token registered on each endpoint** (a prerequisite, not a
   failure). The runner fails fast before transfer if a provided/cached token id
   is not registered on the destination.
8. **polygon → blast / polygon → moonbeam GMP (mainnet) — relayer gas ceiling.**
   Both routes remained in `status=called` for >8 h with no progression. Likely
   cause: the Axelar relayer rejects transactions where the source gas payment
   (POL) undershoots the actual fee on the destination (ETH-gas Blast, GLMR
   Moonbeam). Not a verifier bug; the call never advanced past routing.
9. **celo-sepolia → mantle-sepolia / monad-3 → polygon-sepolia — VotingVerifier
   gap (testnet).** Neither destination chain is covered by the testnet verifier
   set for the specified source. Messages land on Axelar but never receive enough
   votes to advance past `voted`. Not a code issue; the verifier operator set
   must add coverage.
10. **sui → solana GMP — Solana destination rejection.** The Solana destination
    contract rejects GMP execution from Sui sources; `executed_ok: false` in all
    runs. Root: Solana program does not handle Sui-origin message encoding. Axe
    code is correct (ITS sui→sol ✅); limitation is in the example GMP contract.
11. **ITS CANNOT_EXECUTE_MESSAGE/V2 on testnet (multiple routes) — token not
    deployed on destination.** Confirmed via `errorExecute` events in GMP-API:
    - `hyperliquid → monad-3` (ITS testnet): ITS hub cannot route — token not
      registered on monad-3.
    - `stellar-2026-q1-2 → avalanche` (ITS testnet): hub cannot route — token not
      registered on avalanche testnet ITS.
    - `monad-3 → ethereum-sepolia` (ITS testnet): hub cannot route — token not
      registered on ethereum-sepolia ITS.
    Fix: deploy/register the ITS token on each destination before testing. No axe
    code change needed.
12. **solana → hyperliquid ITS (mainnet) — ITS execution failure on Hyperliquid.**
    First-leg hub_approved at T+24 s; no second-leg GMP-API record found. Pattern
    matches CANNOT_EXECUTE (token not deployed on Hyperliquid for the Solana ITS
    token). Needs re-run with token pre-registered on Hyperliquid.
13. **sui → hyperliquid ITS (mainnet) — ITS execution failure on Hyperliquid.**
    Same pattern as above: hub_approved T+23 s, no second-leg found. Likely the
    same token registration gap.

---

## 7. Required GitHub secrets (private RPCs)

Naming: `<CHAIN_DISPLAY_UPPER>_<NETWORK_UPPER>_RPC` (e.g. `KAVA_MAINNET_RPC`).
Values come from the `axelarnetwork/infrastructure` upstreams (they carry API
keys, so they live only in secrets):

- **`BINANCE_*_RPC`** — required; public BNB RPCs block `getLogs`.
- `MOONBEAM_*_RPC`, `KAVA_*_RPC`, `FILECOIN_*_RPC`, `BLAST_*_RPC` — recommended
  (config defaults are flaky / rate-limited).

A run-time `--source-rpc` / `--destination-rpc` always overrides the secret.

---

## 8. Parallelized e2e validation

The route fleet is already scripted and CI-wired:

- `scripts/test_amplifier_routes.sh` — runnable route fleet (cycle-based so each
  chain is exercised as both source and destination).
- `.github/workflows/test-routes.yml` (reusable matrix workflow) called by
  `cron-mainnet.yml` / `cron-testnet.yml` — the twice-daily route fleet, with a
  wallet preflight (`axe check-balances`, native gas + canonical-AXE minimums)
  gating the matrix.

Triage of the §4 open routes is parallelized across child tasks of
[MOU-2](/MOU/issues/MOU-2): QA re-runs each open route and checks on-chain final
state; Protocol Engineer fixes the buffer/discovery bugs that triage confirms and
reconciles the stale `docs/routes.md` / `docs/load-test-coverage.md` with the
23-chain set. AXE_STATE.md §3/§4 are updated as cells resolve.
