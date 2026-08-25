# Supported routes

Which protocols (`GMP`, `ITS`, or both) `axe test load-test` dispatches for a
given (source → destination) pair.

Dispatch is decided by **chain type**, not the specific chain: every EVM chain
shares one EVM row/column, so the matrix below is keyed by type and is the same
on every network. The only thing that varies by network is **which chains are
deployed** — that is the roster in §1. To read a pair, map each chain to its
type via §1, then look it up in §2.

This mirrors [AXE_STATE.md](../AXE_STATE.md) §2. See
[load-test-coverage.md](load-test-coverage.md) for per-cell verified-on-chain
status, env-var requirements, and the outstanding Sui work.

## 1. The 28 wired chains, by type

The authoritative wired set is the `CHAIN_MAP` in
[`.github/actions/run-loadtest/action.yml`](../.github/actions/run-loadtest/action.yml).
The `✓` columns mark the networks each chain is configured for.

| Chain | Type | mainnet | testnet | stagenet | devnet-amplifier |
|---|---|:---:|:---:|:---:|:---:|
| Arbitrum    | EVM     | ✓ | ✓ | ✓ |   |
| Arc         | EVM     |   |   |   | ✓ |
| Avalanche   | EVM     | ✓ | ✓ | ✓ | ✓ |
| Base        | EVM     | ✓ | ✓ | ✓ |   |
| Berachain   | EVM     |   |   | ✓ | ✓ |
| Binance     | EVM     | ✓ | ✓ |   |   |
| Blast       | EVM     | ✓ | ✓ | ✓ |   |
| Celo        | EVM     |   |   | ✓ |   |
| Ethereum    | EVM     | ✓ | ✓ | ✓ | ✓ |
| Filecoin    | EVM     | ✓ | ✓ |   |   |
| Flow        | EVM     |   |   | ✓ | ✓ |
| Hedera      | EVM     | ✓ | ✓ | ✓ |   |
| Hyperliquid | EVM     | ✓ | ✓ | ✓ |   |
| Immutable   | EVM     | ✓ | ✓ |   |   |
| Kava        | EVM     | ✓ | ✓ | ✓ |   |
| Linea       | EVM     | ✓ | ✓ | ✓ |   |
| Mantle      | EVM     | ✓ | ✓ | ✓ |   |
| Monad       | EVM     | ✓ | ✓ |   |   |
| Moonbeam    | EVM     | ✓ | ✓ | ✓ |   |
| Optimism    | EVM     | ✓ | ✓ | ✓ | ✓ |
| Plume       | EVM     |   |   | ✓ | ✓ |
| Polygon     | EVM     | ✓ | ✓ | ✓ |   |
| Scroll      | EVM     | ✓ | ✓ |   |   |
| XRPL EVM    | EVM     | ✓ | ✓ | ✓ | ✓ |
| Solana      | Solana  | ✓ | ✓ | ✓ | ✓ |
| Stellar     | Stellar | ✓ | ✓ |   |   |
| Sui         | Sui     | ✓ | ✓ | ✓ | ✓ |
| XRPL        | XRPL    | ✓ | ✓ | ✓ |   |

XRPL EVM is an ordinary EVM chain (it has a `VotingVerifier` on testnet); only
**XRPL** itself is the contract-less `XRPL` type.

## 2. Dispatch matrix (by chain type)

`GMP+ITS` = both protocols dispatch · `GMP` / `ITS` = only that one ·
`✖` = unavailable by design · `⛔` = wired but bails (not built yet) ·
`⚠️` = dispatches but reverts upstream · `—` = self-pair / n/a.

| source ↓ / destination → | EVM | Solana | Stellar | Sui | XRPL |
|---|---|---|---|---|---|
| **EVM**     | GMP+ITS | GMP+ITS  | GMP+ITS      | GMP+ITS | ITS ✖GMP |
| **Solana**  | GMP+ITS | GMP      | GMP          | GMP+ITS | ✖        |
| **Stellar** | GMP+ITS | GMP ⚠️ITS | —           | GMP+ITS | ✖        |
| **Sui**     | GMP+ITS | GMP+ITS  | ⛔           | —       | ✖        |
| **XRPL**    | ITS     | ✖        | ✖           | ⛔      | —        |

`its-with-data` (transfer + a destination contract call) is wired for
`EVM → Solana` only.

### Reading the special cells

- **EVM → XRPL / XRPL → EVM** — `ITS` only. XRPL has no executable layer, so GMP
  is impossible in any direction touching it; ITS works via the canonical XRP
  wrapper (`xrpl ↔ xrpl-evm` validated).
- **Solana → Solana** — `GMP` only, and it is the *only* same-chain-type route
  axe dispatches. ITS `sol → sol` bails.
- **Solana → Stellar** — `GMP` only; ITS `sol → stellar` bails (not built).
- **Stellar → Solana** — `GMP` runs. ITS dispatches but reverts with
  `Contract Error #7 (UntrustedChain)` until the Stellar ITS contract adds
  `solana` to its trusted chains (upstream fix, no axe change).
- **Sui → Stellar / Sui → XRPL** — both protocols bail (not built). Sui-source
  ITS to EVM/Solana works; the Stellar/XRPL destinations need their verify paths
  wired.
- **Sui → EVM GMP** is code-complete but upstream voter coverage for Sui's
  `Example::gmp::send_call` channel is unreliable; ITS from Sui is unaffected.
- **XRPL → Sui** — bails for both protocols (needs the Sui destination verifier
  plus a registered AXE/XRP token on Sui ITS).

### Notes

- A cell saying a protocol dispatches means axe's *dispatcher* supports it. Live
  success additionally depends on per-chain ITS trusted-chains and token
  registration — see `scripts/test_amplifier_routes.sh` for the runnable fleet.
- Self-pairs (e.g. `Sui → Sui`) are not load-test targets, except `Solana →
  Solana` GMP.
- The matrix is network-independent; a pair is only runnable on a network where
  **both** chains appear in the §1 roster for that network.
