# Testing (`axe test`)

## Single-message tests

### GMP (legacy EVM mode)

```bash
axe test gmp
```

Sends a loopback GMP message on a deployed EVM chain and relays it through the full Amplifier pipeline end-to-end.

### Solana manual relaying

```bash
axe test gmp --network testnet --source-chain solana --destination-chain solana
```

Sends a GMP message from Solana and manually relays it through the full Amplifier pipeline end-to-end: callContract → verify_messages → vote → end_poll → route_messages → construct_proof → approve on Solana gateway (init verification session → verify all 12 signatures → approve message). Requires `MNEMONIC` env var with a funded Cosmos wallet.

Pick the network with `--network` (or point `--config` at a specific chains-config file).

### ITS

```bash
axe test its
```

Deploys an interchain token locally, deploys it remotely to a destination chain via the ITS Hub, then sends a cross-chain transfer and verifies the balance on the destination. Relays through the full Amplifier pipeline (verify → vote → route → execute on hub).

## Load tests

The `axe test load-test` command sends cross-chain transactions through the Axelar Amplifier pipeline and verifies them end-to-end. It supports two modes: **burst** (send N transactions as fast as possible) and **sustained** (send at a fixed TPS rate for a fixed duration).

See [load-test-coverage.md](load-test-coverage.md) for the full source × destination × protocol matrix, per-environment chain availability, and required env vars / RPC overrides per chain.

**Supported pairs**: the authoritative per-type dispatch matrix (which
`(source, destination, protocol)` triples run, bail, or are unavailable by
design) lives in [routes.md](routes.md), with per-cell caveats and validated
status in [load-test-coverage.md](load-test-coverage.md). In short: every
pairing of EVM/Solana/Stellar/Sui runs GMP (and most run ITS, including
Sui ↔ EVM/Solana); XRPL is ITS-only via the canonical XRP wrapper against
XRPL EVM.

**The network is a runtime choice**: pass `--network mainnet | testnet | stagenet | devnet-amplifier` (or set `AXE_NETWORK`). The chains-config is resolved automatically (see [Config resolution](../Readme.md#configuration)); `--config <path>` overrides it, and the binary fails fast if `--network` contradicts the config filename.

### Burst mode (default)

Send a fixed number of transactions all at once. All keys are funded upfront and all transactions are fired in parallel.

```bash
axe test load-test --num-txs 50 ...
```

### Sustained mode

Send transactions at a controlled rate for a set duration. Use `--tps` and `--duration-secs` together:

```bash
axe test load-test --tps 10 --duration-secs 300 ...
# sends 10 tx/s for 5 minutes = 3000 transactions total
```

**How it works:**

- A pool of `tps × key_cycle` wallets is derived and funded upfront (e.g. 30 wallets for 10 TPS with a 3s cycle).
- Each second, `tps` transactions are fired using the next batch of keys from the pool.
- Keys rotate on a configurable cycle (default 3 seconds): second 1 uses keys 0–9, second 2 uses keys 10–19, second 3 uses keys 20–29, second 4 reuses keys 0–9, etc. This ensures each key has time for its previous transaction to land before its nonce is reused.
- Use `--key-cycle N` to control the cycle length. Higher values use more wallets and reduce per-address mempool pressure on chains with aggressive mempool limits (e.g. `--key-cycle 6` doubles the wallet pool).
- **Concurrent send + verify:** In sustained mode, the Amplifier verification pipeline starts immediately as transactions confirm — it does not wait for the send phase to finish. Both phases run concurrently with live progress on separate lines:
  ```
  \ [42/300s]  fired: 420/3000  src-confirmed: 410  failed: 2  (target: 10 tx/s)
  - voted: 350/410  routed: 280/410  approved: 120/410  executed: 80/410
  ```
- The final summary shows end-to-end latency (avg/min/max), throughput, per-phase step and cumulative timing, pipeline counts, and any stuck transactions.
- A JSON report is written to `axe-load-test-logs/axe-load-test-<timestamp>.json` after each run for post-mortem analysis.

**Protocols supported in sustained mode:** EVM, Solana, Stellar, and XRPL sources all run sustained mode (GMP and ITS where the route exists). Sui routes (Sui-source ITS, and Solana/Stellar → Sui ITS) are burst-only by design — Sui's account model serializes submissions.

**ITS note:** Token deployment happens once upfront (cached across runs). Each pool key is pre-funded with enough tokens for its share of the total transfers before the send phase begins.

---

**GMP (default):** Sends ABI-encoded string payloads via `callContract` through a deployed `SenderReceiver` contract. The contract address is cached after first deploy and reused across runs.

**ITS (`--protocol its`):** Deploys an interchain token on the source chain, deploys the remote counterpart on the destination via the ITS Hub, then sends `InterchainTransfer` transactions. Supports both EVM → Sol and Sol → EVM directions.

**Verification:** Polling covers the full pipeline: `voted → routed → approved → executed` (GMP), or `voted → hub-approved → second-leg discovery → routed → approved → executed` (ITS). In sustained mode, verification runs concurrently with sending. In burst mode, it runs after all sends complete. Inactivity timeout is 7200 seconds (2 h, sized for the slowest observed legacy-EVM routes) — the poller resets the timeout each time any transaction makes progress.

### Burst examples

#### GMP: SOL → EVM

```bash
axe test load-test \
  --source-chain solana-18 \
  --destination-chain avalanche-fuji \
  --network devnet-amplifier
```

#### GMP: EVM → SOL

```bash
axe test load-test \
  --source-chain avalanche-fuji \
  --destination-chain solana-18 \
  --num-txs 50 \
  --network devnet-amplifier
```

#### ITS: SOL → EVM

```bash
axe test load-test \
  --source-chain solana-18 \
  --destination-chain avalanche-fuji \
  --protocol its \
  --network devnet-amplifier
```

#### ITS: EVM → SOL

```bash
axe test load-test \
  --source-chain avalanche-fuji \
  --destination-chain solana-18 \
  --protocol its \
  --network devnet-amplifier
```

### Sustained examples

#### GMP: 10 tx/s for 5 minutes, SOL → EVM

```bash
axe test load-test \
  --source-chain solana-18 \
  --destination-chain avalanche-fuji \
  --tps 10 \
  --duration-secs 300 \
  --network devnet-amplifier
```

#### GMP: 5 tx/s for 2 minutes, EVM → SOL

```bash
axe test load-test \
  --source-chain avalanche-fuji \
  --destination-chain solana-18 \
  --tps 5 \
  --duration-secs 120 \
  --network devnet-amplifier
```

#### ITS: sustained EVM → SOL

```bash
axe test load-test \
  --source-chain avalanche-fuji \
  --destination-chain solana-18 \
  --protocol its \
  --tps 3 \
  --duration-secs 180 \
  --network devnet-amplifier
```

### Stagenet / testnet / mainnet

On stagenet/testnet/mainnet the relayer requires gas payment. The same binary serves every network — just pass `--network`:

```bash
cargo install --locked --path .   # or `cargo build --release` + run from target/ when iterating
```

#### Stagenet example

```bash
# Burst
axe test load-test \
  --source-chain flow \
  --destination-chain solana-stagenet-3 \
  --num-txs 100 \
  --network stagenet

# Sustained with larger wallet pool for Flow
EVM_PRIVATE_KEY=0x... axe test load-test \
  --source-chain flow \
  --destination-chain solana-stagenet-3 \
  --tps 2 \
  --duration-secs 120 \
  --key-cycle 6 \
  --network stagenet \
  --source-rpc https://your-flow-rpc-endpoint
```

#### Mainnet examples

Mainnet works for every pair where both chains are deployed there: EVM, Solana, Stellar, Sui, XRPL, and XRPL-EVM. `--network mainnet` resolves all program IDs (Solana gateway/ITS/gas-service/memo) to mainnet automatically.

```bash
# Solana → EVM ITS (or any direction)
axe test load-test \
  --source-chain solana --destination-chain avalanche \
  --num-txs 1 --protocol its \
  --network mainnet

# EVM → Sui GMP (Sui destination verifier polls suix_queryEvents for MessageApproved/MessageExecuted)
axe test load-test \
  --source-chain xrpl-evm --destination-chain sui \
  --num-txs 1 --protocol gmp \
  --network mainnet

# XRPL → XRPL-EVM canonical XRP transfer
axe test load-test \
  --source-chain xrpl --destination-chain xrpl-evm \
  --num-txs 1 --protocol its \
  --network mainnet
```

**Solana commitment**: load-test paths use `CommitmentConfig::finalized` (not `confirmed`) so we don't return from `send_and_confirm_transaction` until the tx is finalized on-chain. This adds ~10–25 s per Solana source tx but eliminates the verifier vote-split race that produced "Failed" polls (5Y / 5N at expiry) on mainnet when verifiers query Solana at different commitments.

**Mainnet vs testnet relay**:
- `axe test load-test` is observe-only — sends source txs and watches the Axelar relayer process them. Works on every environment that has a live relayer (testnet, stagenet, mainnet).
- `axe test gmp` (the legacy single-message mode) manually drives the entire pipeline (vote → end_poll → route → constructProof → approve → execute). Use it for debugging or running without a live relayer. Requires `MNEMONIC` set to a Cosmos wallet funded for Axelar fees.

Run `axe test load-test --help` for all options.

## Configuration

### Per-chain signer keys (load-test)

| Variable | Format | Used by |
|---|---|---|
| `EVM_PRIVATE_KEY` | 32-byte hex (`0x…`) | EVM source flows (signs source tx, derives ephemeral keypair pool, deploys destination `SenderReceiver` on first run) |
| `SOLANA_PRIVATE_KEY` *(optional)* | path to a JSON keypair file (defaults to `~/.config/solana/id.json`) | Solana source flows. Same flag is `--keypair`. |
| `STELLAR_PRIVATE_KEY` | `S…` secret key or 32-byte hex seed | Stellar source flows. Also used as the AXE-receive G-address for `*-to-stellar` ITS. |
| `XRPL_PRIVATE_KEY` *(optional)* | s-prefix family seed (`snr…`, `sh…`) **or** 64-char hex | XRPL source flows. Falls back to `EVM_PRIVATE_KEY` bytes if unset. |
| `SUI_PRIVATE_KEY` | `suiprivkey1…` bech32 (Sui CLI export) — auto-detects ed25519 (flag 0x00) or secp256k1 (flag 0x01) — **or** 32-byte hex (treated as ed25519) | Sui source flows. Get testnet SUI from https://faucet.sui.io |

### RPC overrides (load-test)

| Variable / flag | Effect | Default |
|---|---|---|
| `--source-rpc` / `SOURCE_RPC` | source chain RPC URL | from chain config |
| `--destination-rpc` / `DESTINATION_RPC` | destination chain RPC URL | from chain config |
| `AXELAR_LCD_URL` | Axelar Cosmos REST endpoint (verifier polling) | from chain config; auto-fallback to `lavenderfive` and `publicnode` on 5xx |
| `AXELAR_RPC_URL` | Axelar Tendermint RPC endpoint (`tx_search` for second-leg discovery) | from chain config; auto-fallback to `axelar-rpc.publicnode.com` and `rpc.cosmos.directory/axelar` on 5xx |

The `axe` binary reads `.env` automatically via `dotenvy`, so any of these can be set there or exported in your shell.
