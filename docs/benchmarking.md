# Benchmarking the fee-api cost defaults

`axe bench` runs the gas / compute-unit harnesses that calibrate the fee-api's
`global.toml [cost]` defaults and prints which keys each measurement feeds. It
wraps the harnesses under `benchmarks/`; it does not re-implement the
measurement. Run it from an axe checkout (the harnesses are source projects).

## EVM source gas

```
axe bench evm-gas --rpc <ethereum-mainnet-rpc>
```

Runs `forge test --match-contract GasHarness --isolate` on an Ethereum mainnet
fork (`benchmarks/evm-gas`). `--isolate` meters each top-level call as a real
transaction (intrinsic + calldata + execution against cold state), so the
`gasUsed` per operation equals a receipt's. Feeds `[cost.source_gas_units]`
(gmp / gmpWithToken / itsTransfer / itsTransferWithCall / itsDeployment).
Requires Foundry (`forge`). The RPC also comes from `MAINNET_RPC_URL`.

## Solana compute units

```
axe bench solana-cu
```

Runs the LiteSVM harness (`benchmarks/solana-cu`) against the real mainnet
program binaries, metering `compute_units_consumed`. The `.so` files are not
committed; the command fetches them from mainnet on first run (needs the
`solana` CLI), or run `benchmarks/solana-cu/scripts/fetch-testdata.sh` directly.
Feeds `[cost.solana]` (execution / source / approve compute units, and
`approve_verifier_signatures` = the verify count in one approval batch).

## Both

```
axe bench all
```
