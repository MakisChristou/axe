# Load-test coverage matrix

What `axe test load-test` supports per (source, destination, protocol) combination, and on which Axelar environments. Pairs not listed are unsupported by Axelar today (e.g. XRPL is ITS-only because it has no smart contracts).

## GMP (`callContract`)

| Source ↓ / Dest → | EVM | Solana | Stellar | Sui | XRPL |
|---|---|---|---|---|---|
| **EVM** | ✅ | ✅ | ✅ | ✅ ² | n/a |
| **Solana** | ✅ | ✅ | ✅ | ✅ ² | n/a |
| **Stellar** | ✅ | ✅ | n/a | ✅ ² | n/a |
| **Sui** | ✅ ¹ | ⚠️ ³ | (not built yet) | — | n/a |
| **XRPL** | n/a | n/a | n/a | n/a | n/a |

¹ `sui → <evm>` GMP is validated end-to-end on **mainnet** (`sui → avalanche`, `sui → hyperliquid`). On **testnet** the voter set processing **`Example::gmp::send_call` messages** (sender = `Example.objects.GmpChannelId`) does not consistently vote on them, so testnet runs land on Axelar but rarely progress past "called". **ITS messages from Sui** (sender = `InterchainTokenService.objects.ChannelId`) complete in ~20s on every network.

³ `sui → solana` GMP dispatches and completes the Axelar pipeline, but the example Solana destination program rejects Sui-origin message encoding (`executed_ok: false`). ITS `sui → sol` is unaffected and validated (it runs in the daily mainnet cron).

² **GMP → Sui works end-to-end on testnet**. Verified runs:
- `xrpl-evm → sui` GMP: 73 s (voted 39 s, routed 5 s, approved 19 s, executed 11 s)
- `solana → sui`  GMP: 53 s (voted 25 s, routed 5 s, approved 6 s, executed 16 s)
- `stellar-2026-q1-2 → sui` GMP: 63 s (voted 15 s, routed 5 s, approved 19 s, executed 17 s)

The verifier polls Sui's `events::MessageApproved` and `events::MessageExecuted` on the `AxelarGateway` Move package via cursor-paginated `suix_queryEvents`. Source-side senders are unchanged; destination contract ID is read from `chains.sui.contracts.Example.objects.GmpChannelId`.

The remaining Sui variants (`sui → stellar` / `sui → xrpl` for both protocols, and `xrpl → sui`) are wired in the CLI/dispatch but bail with informative messages identifying what's needed to complete them. See "Outstanding Sui implementation work" below.

XRPL has no contract execution model — it can only carry token payments via ITS, never `callContract`.

## ITS (`interchainTransfer` via hub)

| Source ↓ / Dest → | EVM | Solana | Stellar | Sui | XRPL |
|---|---|---|---|---|---|
| **EVM** | ✅ (evm-to-evm) | ✅ | ✅ ² | ✅ | ✅ canonical XRP |
| **Solana** | ✅ | ❌ bails (sol↔sol) | ❌ not implemented | ✅ | ❌ not implemented |
| **Stellar** | ✅ | ⚠️ ¹ | n/a | ✅ | ❌ not implemented |
| **Sui** | ✅ ³ | ✅ ³ | (not built yet) | — | (not built yet) |
| **XRPL** | ✅ canonical XRP | ❌ not implemented | ❌ not implemented | (not built yet) | n/a |

³ Sui-source ITS requires the test wallet to own a registered `Coin<T>` for the transferred token (the daily cron uses the canonical AXE coin). Burst-mode only.

¹ Module `its_stellar_to_sol` exists and the dispatch is wired, but the **Stellar testnet ITS contract** (`CC7L…M5YP`) has not added `solana` to its trusted-chains list. The run reaches Stellar simulation and reverts with `Contract Error #7 (UntrustedChain)`. The fix is upstream: the contract owner runs `ts-node stellar/its.js add-trusted-chains solana` from `axelar-contract-deployments`. Once that's in, this pair starts working with no code changes here.

² `EVM → Stellar` ITS is validated end-to-end (`avalanche → stellar-2026-q1-2` on testnet, `hyperliquid → stellar` on mainnet). The runner verifies the remote token deploy executed and the token was registered on Stellar before sending transfers, and fails before transfer if a provided/cached token id is not registered on Stellar ITS. The default EVM→Stellar gas value is `1000000000000000000` wei; override it with `--gas-value` only when testing a different funding assumption.

## Outstanding Sui implementation work

✅ **DONE — Sui destination + EVM/Sol/Stellar → Sui GMP** (`DestinationChecker::Sui`, cursor-paginated `suix_queryEvents` on `events::MessageApproved` / `events::MessageExecuted` from the `AxelarGateway` Move package).

✅ **DONE — Sui-source ITS to EVM/Solana** (`its_sui_to_evm`, `its_sui_to_sol`) and **ITS to Sui from EVM/Solana/Stellar** (`its_evm_to_sui`, `its_sol_to_sui`, `its_stellar_to_sui`). Sui-source ITS resolves the coin type via `registered_coin_type` dev-inspect and requires the wallet to own a registered `Coin<T>` (the daily mainnet cron runs `sui → solana` and `solana → sui` ITS with the canonical AXE coin). Sui routes are burst-only by design — the account model serializes submissions.

What's left:

1. **ITS `sui → stellar` / `sui → xrpl`, GMP `sui → stellar`, and `xrpl → sui`** — dispatch arms exist but bail with explanatory messages; the destination verify paths (and for XRPL a registered token on Sui ITS) still need wiring.
2. **Sui-source GMP voter coverage (testnet)** — `Example::gmp::send_call` messages from Sui's `GmpChannelId` are rarely voted on by the testnet verifier set. Mainnet coverage is fine (`sui → avalanche` / `sui → hyperliquid` GMP validated). Mitigation lives upstream — no axe changes needed.

## Per-environment chain availability

Whether a pair works also depends on whether both chains are deployed on the chosen environment. For the exact per-network roster of all 28 wired chains (the EVM "many" expanded out), see [routes.md §1](routes.md#1-the-23-wired-chains-by-type).

| Env | EVM chains | Solana | Stellar | Sui | XRPL | XRPL-EVM | Notes |
|---|---|---|---|---|---|---|---|
| **testnet** | many | `solana` | `stellar-2026-q1-2` | `sui` | `xrpl` | `xrpl-evm` | most coverage |
| **stagenet** | many | `solana-stagenet-3` | none | `sui` | `xrpl` | `xrpl-evm` | no Stellar |
| **devnet-amplifier** | `avalanche-fuji`, `eth-sepolia`, `optimism-sepolia`, `arc-2`, `berachain`, `flow`, `plume-2` | `solana-18` | none | `sui-2` | none | `xrpl-evm-devnet` ⚠️ | no Stellar; xrpl-evm-devnet AxelarGateway/ITS not deployed at configured addresses (`eth_getCode` returns `0x`) — GMP/ITS to it falls through pre-flight bytecode check |
| **mainnet** | many | `solana` | `stellar` | `sui` | `xrpl` | `xrpl-evm` | full coverage; `--network mainnet` resolves the Solana program IDs to mainnet (`gtwqvLL…`, `gaszjG…`, `memtaCu…`, `itsAUdH…`) |

## Resolving how a `--protocol`/`--source-chain`/`--destination-chain` triple is dispatched

Auto-detect runs through these steps in order:
1. If both `--source-chain` and `--destination-chain` are provided, the chain types are read from the config (`chainType: evm | svm | stellar | xrpl`) and combined into a `TestType`.
2. Otherwise, the user-provided `--test-type` is honored.
3. The combined `(Protocol, TestType)` selects an `its_*` or `run_*` runner in [`src/commands/load_test.rs`](../src/commands/load_test.rs).

## Required env vars / flags by chain

| Chain | What's needed | Where |
|---|---|---|
| EVM (any) | `EVM_PRIVATE_KEY` (32-byte hex secp256k1) | `.env` or `--private-key` |
| Solana | `SOLANA_PRIVATE_KEY` pointing at a JSON keypair file (defaults to `~/.config/solana/id.json`) | `.env` or `--keypair` |
| Stellar | `STELLAR_PRIVATE_KEY` (`S…` secret or 32-byte hex) — used as both signer **and** receiver in stellar-to-* flows | `.env` |
| Sui | `SUI_PRIVATE_KEY` (`suiprivkey1…` bech32 from `sui keytool export` — supports both ed25519 (flag 0x00) and secp256k1 (flag 0x01); also accepts a 32-byte hex secret as ed25519). Get testnet SUI from https://faucet.sui.io | `.env` |
| XRPL (sender) | `XRPL_PRIVATE_KEY` (s-prefix family seed, e.g. `snr…` — falls back to `EVM_PRIVATE_KEY` bytes if unset) | `.env` |
| XRPL (receiver) | hardcoded per network in `src/commands/load_test/routes/its_evm_to_xrpl.rs`. Mainnet receiver is the address derived from the operator's XRPL_PRIVATE_KEY; testnet/devnet/stagenet share a separate hardcoded address. Override is intentional, not via flag — change the const. |

## RPC overrides

| Override | Effect | Default |
|---|---|---|
| `--source-rpc` / `SOURCE_RPC` | source chain RPC URL | from chain config |
| `--destination-rpc` / `DESTINATION_RPC` | destination chain RPC URL | from chain config |
| `AXELAR_LCD_URL` | Axelar Cosmos REST endpoint | from chain config; auto-falls back to `lavenderfive` and `publicnode` on 5xx |
| `AXELAR_RPC_URL` | Axelar Tendermint RPC endpoint | from chain config; auto-falls back to `axelar-rpc.publicnode.com` and `rpc.cosmos.directory/axelar` on 5xx |

## All `axe test load-test` flags

Every flag is optional. Defaults are picked from the chain config and `--network`.

| Flag | Type | Notes |
|---|---|---|
| `--config <path>` | path | Optional override; the config is otherwise resolved from `--network` (checkout → cache → GitHub fetch). A filename naming a different network is a hard error. |
| `--source-chain <axelarId>` | string | Auto-detected when only one chain of the source type exists. Required for Sui pairs and any ambiguous cases. |
| `--destination-chain <axelarId>` | string | Same as source. |
| `--test-type <enum>` | one of `sol-to-evm \| evm-to-sol \| evm-to-evm \| sol-to-sol \| xrpl-to-evm \| evm-to-xrpl \| stellar-to-evm \| evm-to-stellar \| stellar-to-sol \| sol-to-stellar \| sui-to-evm \| evm-to-sui \| sol-to-sui \| sui-to-sol \| stellar-to-sui \| sui-to-stellar \| xrpl-to-sui \| sui-to-xrpl` | Auto-detected from the chain types — only set this if you want to override. |
| `--protocol <gmp \| its \| its-with-data>` | enum | Default `gmp`. `its-with-data` only supports `evm-to-sol`. |
| `--num-txs <N>` | u64 | Burst-mode tx count (default 1). |
| `--tps <N>` + `--duration-secs <N>` | u64 | Sustained-mode (EVM, Solana, Stellar, and XRPL sources; Sui routes are burst-only). Pool size = `tps × key_cycle`. |
| `--key-cycle <N>` | u64 | Sustained-mode wallet rotation (default 3). Higher reduces per-address mempool pressure. |
| `--source-rpc <url>` / `--destination-rpc <url>` | string | Override the per-chain RPC URLs from config. Also via `SOURCE_RPC` / `DESTINATION_RPC` env. |
| `--private-key <hex>` | string | EVM key. Also via `EVM_PRIVATE_KEY` env. |
| `--keypair <path>` | path | Solana JSON keypair. Also via `SOLANA_PRIVATE_KEY` env. Defaults to `~/.config/solana/id.json`. |
| `--gas-value <wei/lamports/stroops/mist>` | string | Cross-chain gas attached to the source-side message. Default per source chain. |
| `--token-id <hex>` | string | Skip auto-token-deploy and use an existing ITS token id (e.g. canonical XRP `0xba5a21ca…`). |
| `--payload <hex>` | string | Override the auto-generated payload. |
| `--extra-accounts <N>` | u32 | Solana ITS-with-data only — extra accounts in the executable payload. |

## Solana commitment level

All Solana RPC clients in load-test paths (sender, verifier, keypairs) use `CommitmentConfig::finalized` since [src/solana.rs](../src/solana.rs) and the load-test modules. Earlier we used `confirmed` (faster ~5 s vs ~13–30 s) but this caused vote splits on mainnet: some Axelar verifiers query Solana at `confirmed`, others at `finalized`, so a tx confirmed-but-not-finalized could be voted on at mixed visibility — leading to `5Y / 5N` polls expiring as `Failed`. Finalized adds latency to the source confirm step but eliminates the race; net end-to-end time is roughly unchanged because the Axelar voter pass is faster when all queries see consistent state.

The `decode tx <solana-signature>` and `decode sol-activity` subcommands stay on `confirmed` — read-only diagnostic paths where the lower-latency commitment doesn't risk consistency.

## Picking the environment

One binary serves every network — select it at runtime with `--network
mainnet | testnet | stagenet | devnet-amplifier` (or `AXE_NETWORK`). Program
IDs and chain configs resolve from that choice. Passing a `--config` file
that names a different network is a hard error:

```
Error: --network mainnet contradicts the config file (testnet); pass a matching --config or drop one
```
