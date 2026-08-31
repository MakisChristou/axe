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

Unlike the EVM harness, this one does **not** feed `[cost.solana]` directly. It
runs on minimal injected state and lands about 25% under real mainnet, so it is
the tool for the per-CPI breakdown and for catching a regression after a program
redeploy. The config values come from:

```
python3 benchmarks/solana-cu/scripts/mainnet_cu_limits.py
```

which reads the compute-unit limits real mainnet transactions carry, per
operation variant. That limit, not consumption, is what Solana charges: the
priority fee is paid on the requested budget and the remainder is never
refunded. Feeds `[cost.solana]` (execution / source / approve compute units, and
`approve_verifier_signatures` = the verify count in one approval batch).

## Both

```
axe bench all
```
