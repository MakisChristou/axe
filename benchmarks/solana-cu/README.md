# Solana ITS compute-unit (CU) harness

The Solana analog of the Foundry EVM `GasHarness.t.sol`. It measures
`compute_units_consumed` for the ITS operations the fee-api prices on Solana and
compares them against the observed mainnet numbers.

The programs under test are the real mainnet binaries (ITS, gateway, gas-service,
Metaplex Token Metadata, and — for the WSOL-unwrap flow — native-unwrapper), run
in-process under LiteSVM so the measured CU reflects the deployed bytecode rather
than the calling crate. They are not committed (~2 MB, and they go stale on
redeploy); fetch them from mainnet first. See "Fetching the testdata" below.

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

`scripts/fetch-testdata.sh` dumps the five mainnet programs (ITS, gateway,
gas-service, native-unwrapper, Metaplex Token Metadata) into
`tests/testdata/*.so` with `solana program dump`. Re-run it after a mainnet
redeploy. Requires the `solana` CLI + network; override the RPC with
`SOLANA_RPC_URL`.

Set `CU_LOGS=1` to also print the per-CPI CU breakdown from the program logs:

```
CU_LOGS=1 cargo test -p its-cu-harness -- --nocapture --test-threads=1
```

This crate is a detached workspace (its own `[workspace]` table) so it can pin
the `mainnet` network feature independently of the parent `native-unwrapper`
program, which builds with `devnet-amplifier`.

## Consumed vs charged

Solana charges the priority fee on the compute-unit **limit** a transaction
requests, not on what it consumes — unused units are never refunded. So the
number the fee-api prices is the budget the submitter sets, and every row below
reports both.

On the destination legs the submitter is the relayer, whose policy lives in
`axelar-relayer-solana` `src/gas_calculator.rs`: `initialize_payload_verification_session`
and `verify_signature` skip simulation and send a hardcoded constant, everything
else simulates and adds 25%. Those 25% are charged whether or not they are used,
so they do **not** stand in for the fee-api's own safety margin — the relayer's
buffer absorbs contention between simulation and inclusion, the API's absorbs
drift between the quote and the transaction.

## This harness is not the calibration source

`scripts/mainnet_cu_limits.py` is. It reads the compute-unit limits real mainnet
transactions carry, per operation variant, and the fee-api's `[cost.solana]`
budgets are read off its CHARGED max column:

```
python3 scripts/mainnet_cu_limits.py
SOLANA_RPC_URL=<faster endpoint> python3 scripts/mainnet_cu_limits.py
```

The public endpoints are slow enough that the rarer variants (`Execute / ata+`,
`Execute / withcall+ata+`) may not appear; the script says so when they are
missing rather than letting you read a table with holes in it.

This LiteSVM harness measures the same operations against the same deployed
bytecode, but on minimal injected state, and it lands consistently **under** real
mainnet:

| Operation | Harness consumed | Mainnet consumed | Harness / mainnet |
|---|---:|---:|---:|
| destination `execute`, ATA exists | 86,318 | 114,522 | 0.75 |
| destination `execute`, ATA created | 106,055 | 140,771 | 0.75 |
| destination itsTransferWithCall | 158,268 | 169,293 | 0.93 |
| source `interchain_transfer` | 56,540 | 74,121 | 0.76 |

So use this harness to answer *why* a number is what it is — the per-CPI
breakdown, the cost of one code path against another, and as a regression check
when a program is redeployed. Do not copy its numbers into config. The one
operation with no mainnet sample (destination itsDeployment) is the exception,
and it carries the 0.75 scaling as an explicit correction.

The cause of the 25% gap is not yet isolated. It is **not** Token-2022 extensions
on the mainnet mint, which an earlier version of this file claimed: the real ITS
mints are 82-byte base mints with no extensions, exactly what the harness
injects. Remaining candidates are the real token manager's flow-limit accounting
(injected here as `flow_limit: None`) and larger real-world message payloads.

## Results

Values exclude the fixed 150 CU of the prepended `SetComputeUnitLimit`
instruction.

| Operation | Consumed | Charged | Notes |
|---|---:|---:|---|
| source `interchain_transfer` (itsTransfer, `data=None`) | 56,540 | 70,675 | burn + gateway `call_contract` + gas `pay_gas`; mainnet target ~67k |
| source `interchain_transfer` (itsTransferWithCall, `data=Some(128B)`) | 58,834 | 73,542 | same instruction, payload also carries a keccak-hashed data blob |
| source `register_canonical_interchain_token` | 48,192 | 60,240 | local; creates the lock/unlock token manager, no GMP |
| source `deploy_remote_canonical_interchain_token` | 49,456 | 61,820 | the GMP-emitting deploy |
| source itsDeployment (both, combined) | 97,648 | 122,060 | two transactions, so two budgets |
| source gmp: gateway `call_contract` | 8,803 | 11,003 | bare contract call from a wallet |
| destination `execute`, recipient ATA exists | 86,318 | 107,897 | gateway `validate_message` + ITS give-token/`MintTo` |
| **destination `execute`, recipient ATA created** | **106,055** | **132,568** | the `init_if_needed` branch; what a quote must budget for |
| destination itsTransferWithCall (WSOL unwrap), ATA exists | 138,170 | 172,712 | ITS execute + give-token + `native-unwrapper` CPI |
| **destination itsTransferWithCall (WSOL unwrap), ATA created** | **158,268** | **197,835** | as above, plus the ATA the unwrapper then closes |
| **destination itsDeployment** (inbound `DeployInterchainToken`) | **182,119** | **227,648** | token manager + Token-2022 mint + manager ATA + minter roles + Metaplex `CreateV1` CPI |

The bolded rows are the variants the fee-api budgets for. Their *values* come from
`mainnet_cu_limits.py`, not from this table.

### The destination ATA is not free

Both destination transfer paths run `init_if_needed` on the recipient ATA
(`ExecuteInterchainTransfer::destination_ata`), so a first-time recipient makes
the relayer create the account inside `execute`. That costs ~20k CU on top of the
transfer itself, and a quote cannot know in advance which branch will run, so the
created case is the one the fee-api budgets. (Its *rent* is separate and is
already priced from the request-path ATA probe.)

The gateway message-approval flow (Niko's ~3.1M) is measured separately from
real mainnet transactions, not under LiteSVM, because it spans many
transactions (see below). Per-instruction mainnet medians:

| Gateway approval instruction | Mainnet CU (median) | Frequency |
|---|---:|---|
| `initialize_payload_verification_session` | ~11,000 | once per payload batch |
| `verify_signature` | ~198,900 | once per signer to reach threshold, per batch |
| `approve_message` | ~49,800 | once per message |
| **one full approval batch** (1 init + 12 verify + 1 approve) | **~2,449,000** | per batch |

What the relayer *budgets* for those, though, is not the median it consumes:

| Gateway approval instruction | Charged (CU) | How |
|---|---:|---|
| `initialize_payload_verification_session` | 30,000 | hardcoded |
| `verify_signature` | 220,000 | hardcoded |
| `approve_message` | 62,250 | simulated + 25% |

The fee-api models the workflow as `approve_verifier_signatures + 2` transactions
that all share one `approve_compute_units`, so that field takes the **largest**
per-transaction budget above — 220,000, the hardcoded `verify_signature` — rather
than the ~199k it typically consumes. `gateway_approval_charged_budget` in the
harness records this derivation.

### Source transfer vs mainnet

The measured 56.5k breaks down (from the CU logs) as roughly: gateway
`call_contract` 10.1k, gas `pay_gas` 9.4k, Token-2022 burn 1.2k, and the rest ITS
logic and event emission. Real mainnet transfers consume ~74.1k, the same ~25%
gap the destination paths show; the dominant cost structure is reproduced but the
absolute number is not. See "This harness is not the calibration source" above.

Note that consumption is not what a source transfer is charged either way: just
over half of real ones set no compute-unit limit, so they pay Solana's 200k
default allocation, and the largest explicit limit observed is also 200k.

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
treasury). Measured end-to-end at 138,170 CU with the unwrapper's ATA already in
place, of which the gateway `validate_message` CPI is ~26.7k and the
`native-unwrapper` CPI (close + split) is ~21.2k; the rest is ITS execute, the
give-token transfer, and event emission. Creating that ATA first adds ~20.1k, for
158,268 CU. This replaces the conservative
`execution_compute_units.itsTransferWithCall` guess.

### Destination itsDeployment

An inbound `DeployInterchainToken` is the most expensive destination operation:
one `execute` creates the token manager, the Token-2022 mint, the token manager's
ATA and the minter's `UserRoles` account, and CPIs Metaplex `CreateV1` for the
mint's metadata. Measured at 182,119 CU — an order of magnitude under the 1.4M
transaction cap the fee-api used as a placeholder for it. Measured with a minter
present, since that deployment also creates the roles account.

The rents for those accounts are priced separately, from
`[cost.solana.account_bytes.deployment]`, and are the dominant lamport term; this
number is only the compute budget.

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

On current mainnet each batch carries exactly one message, so the full batch
lands on that single message. In charged terms the full inbound delivery of one
ITS message today is roughly:

```
30k (init) + 12 x 220k (verify) + 62k (approve)  +  133k (ITS execute)  ~=  2.87M CU
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

1. Loads the mainnet `.so` binaries at their real program IDs.
2. Injects the prerequisite state directly with `set_account` rather than running
   the init/deploy instructions: the gateway root config, gas-service treasury,
   ITS root config, a native interchain token (token manager + Token-2022 mint +
   ATAs), and, for the inbound path, an already-approved `IncomingMessage` whose
   stored hash matches `message.hash()` (Route B). Accounts the instruction under
   test is supposed to create are deliberately left out.
3. Builds the single instruction under test (using the crate's own
   `make_interchain_transfer_instruction` helper where available) and sends it
   with a raised compute-unit limit.
4. Reads `compute_units_consumed` from the transaction metadata and converts it
   to the charged budget (see "Consumed vs charged").

State structs are serialized with their real crate types (Anchor
`AccountSerialize` for borsh accounts, `bytemuck` for zero-copy accounts) so the
injected bytes match the on-chain layout exactly.
