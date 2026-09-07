# axe

[![CI](https://github.com/axelarnetwork/axe/actions/workflows/ci.yml/badge.svg)](https://github.com/axelarnetwork/axe/actions/workflows/ci.yml)

Swiss army knife CLI for Axelar cross-chain development: send and verify GMP/ITS
messages end-to-end, run load tests, decode transactions, monitor verifiers, and
submit governance proposals — across EVM, Solana, Stellar, Sui, and XRPL.

## Install

```bash
cargo install --locked --git https://github.com/axelarnetwork/axe axe
```

One binary serves all four networks (`mainnet | testnet | stagenet |
devnet-amplifier`) — pick one with the global `--network` flag or the
`AXE_NETWORK` env var. Chain configs are fetched and cached automatically; no
checkout or setup required.

## Usage

```bash
# Send a GMP message Solana → Solana and relay it through the full Amplifier pipeline
axe test gmp --network testnet --source-chain solana --destination-chain solana

# Cross-chain load test: 50 transactions, verified end-to-end
axe test load-test --network testnet --source-chain solana --destination-chain flow --num-txs 50

# Decode any Axelar-related transaction or calldata
axe decode tx 0xabc123...
axe decode calldata 0x0f4433d3...

# Recent on-chain activity of the Axelar Solana programs
axe decode sol-activity --network mainnet --limit 5

# Who verifies a chain, and how did a verifier vote recently?
axe verifiers mainnet solana
axe verifier-votes mainnet solana axelar1s2cf963rm0u6kxgker95dh5urmq0utqq3rezdn

# Submit (and relay) a governance proposal to an edge chain
axe propose testnet avalanche --op pause --relay
```

## Commands

| Command                   | Description                                            | Docs |
| ------------------------- | ------------------------------------------------------ | ---- |
| `axe test gmp`            | End-to-end GMP test (manual relay supported)           | [testing](docs/load-testing.md) |
| `axe test its`            | Deploy + transfer an interchain token                  | [testing](docs/load-testing.md) |
| `axe test load-test`      | Burst / sustained cross-chain load test                | [testing](docs/load-testing.md) |
| `axe test express-execution` | Express-reimbursement monitor (observe-only)        | — |
| `axe decode calldata`     | Decode raw EVM calldata (embedded ABI database)        | [decode](docs/decode.md) |
| `axe decode tx`           | Fetch & decode an EVM or Solana transaction            | [decode](docs/decode.md) |
| `axe decode sol-activity` | Recent Solana program activity                         | [decode](docs/decode.md) |
| `axe decode evm-activity` | Recent EVM contract events                             | [decode](docs/decode.md) |
| `axe verifiers`           | List active verifiers for a chain                      | [monitoring](docs/monitoring.md) |
| `axe verifier-votes`      | Recent votes cast by a single verifier                 | [monitoring](docs/monitoring.md) |
| `axe its-ownership`       | ITS owner/operator table for a network                 | [monitoring](docs/monitoring.md) |
| `axe propose`             | Submit & optionally relay an ASG governance proposal   | [governance](docs/governance.md) |
| `axe deploy ...`          | Deploy Axelar contracts to a new chain                 | [deploying](docs/deploying.md) |
| `axe info block`          | Block height/timestamp lookup & prediction             | — |
| `axe check-balances`      | Pre-flight wallet balance check for load tests         | — |

## Configuration

Commands need the chains-config JSON from
[axelar-contract-deployments](https://github.com/axelarnetwork/axelar-contract-deployments).
axe resolves it automatically, in this order:

1. Explicit `--config <path>` flag or `CHAINS_CONFIG` env var
2. A sibling checkout at `../axelar-contract-deployments/axelar-chains-config/info/<network>.json`
3. A cached copy (refreshed when older than 24h):
   `~/Library/Application Support/axe/chains-config/` on macOS,
   `~/.local/share/axe/chains-config/` on Linux
4. Fetched from GitHub and cached

Delete the cache file (or pass `--config`) to force a refresh. A `--network`
flag that contradicts the `--config` filename is a hard error.

Secrets and overrides are read from the environment (a `.env` in the working
directory is loaded automatically — see [`.env.example`](.env.example)):

| Variable | Used by |
| --- | --- |
| `MNEMONIC` | `test gmp` / `test its` manual relaying, `propose` (funded Axelar account) |
| `EVM_PRIVATE_KEY`, `SOLANA_PRIVATE_KEY`, `STELLAR_PRIVATE_KEY`, `SUI_PRIVATE_KEY`, `XRPL_PRIVATE_KEY` | load-test source flows ([details](docs/load-testing.md#configuration)) |
| `SOURCE_RPC`, `DESTINATION_RPC`, `AXELAR_LCD_URL`, `AXELAR_RPC_URL` | RPC overrides |
| `SEP_RPC_URL`, `ARB_SEPOLIA_RPC_URL`, `BASE_SEPOLIA_RPC_URL`, `FUJI_RPC_URL` | Intent testnet RPC overrides; validated first, then chains-config fallback |
| `ALCHEMY_TOKEN` *(optional)* | `decode tx` archive RPCs |
| `CHAIN`, `ENV`, `DEPLOYER_PRIVATE_KEY`, `TARGET_JSON`, … | `deploy` ([details](docs/deploying.md)) |

`axe deploy` is the one command that needs a real local checkout of
axelar-contract-deployments (it writes back into the config) — see
[docs/deploying.md](docs/deploying.md).

## Documentation

- [Supported routes](docs/routes.md) — which chain pairs and protocols work, per network
- [Testing & load testing](docs/load-testing.md) — single messages, burst/sustained modes, examples, per-chain keys
- [Load-test coverage matrix](docs/load-test-coverage.md) — dispatcher support by chain type
- [Decoding](docs/decode.md) — calldata, transactions, on-chain activity
- [Monitoring](docs/monitoring.md) — verifiers, votes, ITS ownership
- [Governance proposals](docs/governance.md) — `axe propose` catalog, flags, relay flow
- [Deploying a new chain](docs/deploying.md) — the 23-step deploy pipeline
- [Debugging cross-chain messages](docs/axelar-debugging.md) — tracing GMP/ITS through the pipeline
- [Useful queries](docs/useful-queries.md) — copy-paste contract queries

## Development

```bash
git clone https://github.com/axelarnetwork/axe && cd axe
cargo build                        # debug build
cargo install --locked --path .    # install your working tree
git config core.hooksPath .githooks  # fmt + clippy + tests on commit
```

## License

[MIT](LICENSE)
