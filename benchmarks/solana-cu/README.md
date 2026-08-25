# Solana ITS compute-unit (CU) harness

The Solana analog of the Foundry EVM `GasHarness.t.sol`. It measures
`compute_units_consumed` for the ITS operations the fee-api prices on Solana and
compares them against the observed mainnet numbers.

The programs under test are the real mainnet binaries (ITS, gateway, gas-service,
and — for the WSOL-unwrap flow — native-unwrapper), run in-process under LiteSVM
so the measured CU reflects the deployed bytecode rather than the calling crate.
They are not committed (~2 MB, and they go stale on redeploy); fetch them from
mainnet first. See "Fetching the testdata" below.

## Running

Via axe (fetches the mainnet `.so` automatically when missing):

```
axe bench solana-cu
```

Or directly (fetch once, then run):

```
scripts/fetch-testdata.sh
cargo test -p its-cu-harness -- --nocapture
```

### Fetching the testdata

`scripts/fetch-testdata.sh` dumps the four mainnet programs (ITS, gateway,
gas-service, native-unwrapper) into `tests/testdata/*.so` with `solana program
dump`. Re-run it after a mainnet redeploy. Requires the `solana` CLI + network;
override the RPC with `SOLANA_RPC_URL`.

Set `CU_LOGS=1` to also print the per-CPI CU breakdown from the program logs:

```
CU_LOGS=1 cargo test -p its-cu-harness -- --nocapture --test-threads=1
```

This crate is a detached workspace (its own `[workspace]` table) so it can pin
the `mainnet` network feature independently of the parent `native-unwrapper`
program, which builds with `devnet-amplifier`.

## Results

| Operation | Measured (CU) | Mainnet target | Notes |
|---|---:|---:|---|
| source `interchain_transfer` (itsTransfer, `data=None`) | 56,540 | ~67k | burn + gateway `call_contract` + gas `pay_gas` |
| source `interchain_transfer` (itsTransferWithCall, `data=Some(128B)`) | 58,834 | ~67k | same instruction, payload also carries a keccak-hashed data blob |
| source `register_canonical_interchain_token` | 48,192 | part of itsDeployment | local; creates the lock/unlock token manager, no GMP |
| source `deploy_remote_canonical_interchain_token` | 49,456 | part of itsDeployment | the GMP-emitting deploy; the fee-api source cost for a deployment |
| source itsDeployment (both, combined) | 97,648 | ~90k (old guess) | register + remote deploy |
| source gmp: gateway `call_contract` | 8,803 | n/a | bare contract call from a wallet |
| destination `execute` (inbound give-token) | 86,318 | see below | gateway `validate_message` (26,588) + ITS give-token/`MintTo` (30,294) |
| destination itsTransferWithCall (WSOL unwrap) | 137,897 | replaces old guess | full ITS execute + give-token + `native-unwrapper` CPI |

Values exclude the fixed 150 CU of the prepended `SetComputeUnitLimit`
instruction.

The gateway message-approval flow (Niko's ~3.1M) is measured separately from
real mainnet transactions, not under LiteSVM, because it spans many
transactions (see below). Per-instruction mainnet medians:

| Gateway approval instruction | Mainnet CU (median) | Frequency |
|---|---:|---|
| `initialize_payload_verification_session` | ~11,000 | once per payload batch |
| `verify_signature` | ~198,900 | once per signer to reach threshold, per batch |
| `approve_message` | ~49,800 | once per message |
| **one full approval batch** (1 init + 12 verify + 1 approve) | **~2,449,000** | per batch |

### Source transfer vs the ~67k target

The measured 56.5k breaks down (from the CU logs) as roughly: gateway
`call_contract` 10.1k, gas `pay_gas` 9.4k, Token-2022 burn 1.2k, and the rest
ITS logic and event emission. The residual gap to the ~67k mainnet figure is
consistent with the Token-2022 metadata extensions carried by the real mainnet
interchain-token mint (this harness injects a base mint with no extensions) plus
larger real-world payloads. The dominant cost structure is reproduced.

### Source itsDeployment (canonical)

The app deploys via `deploy-its-solana.ts`, which runs two ITS instructions:
`register_canonical_interchain_token` (local, creates the lock/unlock token
manager, no GMP) then `deploy_remote_canonical_interchain_token` (emits the GMP
message). Measured at 48,192 and 49,456 CU, so 97,648 combined. The remote
deploy is the one the fee-api prices as the deployment source cost (its
`call_contract` + `pay_gas` CPIs cost the same ~19k as an outbound transfer).
Both read the token's name/symbol from an MPL metadata account (injected here).

### Destination itsTransferWithCall (WSOL unwrap)

The app's real with-call destination is the `native-unwrapper` program. ITS
delivers WSOL to it (a lock/unlock canonical token), then CPIs into
`execute_with_interchain_token`, which closes the WSOL ATA into a program escrow
and splits the lamports (the `amount` to the recipient, the ATA rent to the rent
treasury). Measured end-to-end at 137,897 CU, of which the gateway
`validate_message` CPI is ~26.7k and the `native-unwrapper` CPI (close + split)
is ~21.2k; the rest is ITS execute, the give-token transfer, and event emission.
This replaces the conservative `execution_compute_units.itsTransferWithCall`
guess.

### Destination execute and the 2.08M / 3.1M targets

The ITS `execute` instruction itself costs only ~86k CU. It CPIs the gateway
`validate_message` (which just marks an already-approved message executed) and
then mints the token to the recipient.

The ~2.08M and ~3.1M mainnet figures are not this instruction. They are the
gateway's message-approval flow that runs first: initializing a payload
verification session, verifying secp256k1 signatures over the Axelar verifier
set, and approving the message. That secp256k1 verification is where the
millions of CU go, and because a single Solana transaction is capped at 1.4M CU,
the flow is split across many transactions on mainnet (it cannot run in one
LiteSVM transaction).

`scripts/gateway_approval_cu.py` measures this directly from recent mainnet
gateway transactions (reading `computeUnitsConsumed` and reconstructing complete
approval batches). Across 6 consecutive batches the numbers are stable:

* Each batch is `1 x init` + `12 x verify_signature` + `1 x approve_message`.
* `verify_signature` is ~198,900 CU each and dominates: 12 of them is ~2.385M.
* One full approval batch totals ~2.45M CU (min 2,446,141, median 2,449,086).

The 12 `verify_signature` transactions equal the number of verifier-set
signatures needed to reach the current threshold. Niko's ~3.1M corresponds to a
larger threshold (roughly 15 signatures: 15 x 198,900 + init + approve is about
3.05M). The threshold changes as the Axelar verifier set rotates, so the total
tracks the signature count; the ~2.45M measured here and Niko's ~3.1M are the
same flow at different verifier-set sizes.

### Per-message vs amortized (for fee-api quoting)

The expensive part (`init` + N x `verify_signature`, about 2.4M CU) is charged
**per payload batch** (one payload merkle root), not per message. It is
amortized across every message approved under that root. Only `approve_message`
(~50k) is per message, and the ITS `execute` (~86k) is per message.

On current mainnet each batch carries exactly one message, so the full ~2.45M
lands on that single message. The full inbound delivery of one ITS message today
is therefore roughly:

```
~2.45M (approval: init + 12 verify + approve)  +  ~86k (ITS execute)  ~=  2.54M CU
```

split across ~15 transactions. If Axelar batched M messages under one root, the
per-message cost would drop toward `(init + N x verify)/M + approve + execute`.
The fee-api should model the verification cost as per-batch divided by the
expected batch size (currently ~1), plus a per-message `approve` + `execute`.

### gmpWithToken

There is no distinct Solana ITS instruction for an EVM-style
`callContractWithToken` / gmpWithToken. The equivalent is
`interchain_transfer` with `data = Some(..)` (the itsTransferWithCall row above),
which carries both the token transfer and an arbitrary payload.

## How it works

For each operation the harness:

1. Loads the three mainnet `.so` binaries at their real program IDs.
2. Injects the prerequisite state directly with `set_account` rather than running
   the init/deploy instructions: the gateway root config, gas-service treasury,
   ITS root config, a native interchain token (token manager + Token-2022 mint +
   ATAs), and, for the inbound path, an already-approved `IncomingMessage` whose
   stored hash matches `message.hash()` (Route B).
3. Builds the single instruction under test (using the crate's own
   `make_interchain_transfer_instruction` helper where available) and sends it
   with a raised compute-unit limit.
4. Reads `compute_units_consumed` from the transaction metadata.

State structs are serialized with their real crate types (Anchor
`AccountSerialize` for borsh accounts, `bytemuck` for zero-copy accounts) so the
injected bytes match the on-chain layout exactly.
