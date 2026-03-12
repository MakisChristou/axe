# axe

Swiss army knife CLI for Axelar development.

## Quick Start

```bash
# 1. Clone the contract deployments repo as a sibling
git clone https://github.com/axelarnetwork/axelar-contract-deployments.git
cd axelar-contract-deployments && npm install && cd ..

# 2. Configure
cp axe/.env.example axe/.env
# Edit .env with your chain details, keys, and mnemonics

# 3. Install
cd axe
cargo install --path .

# 4. Initialize and deploy
axe init
axe deploy
```

```
workspace/
├── axe/
└── axelar-contract-deployments/
```

## Commands

| Command              | Description                                   | Relayer |
| -------------------- | --------------------------------------------- | ------- |
| `axe init`           | Initialize a new chain deployment from `.env` | -       |
| `axe deploy`         | Run all deployment steps sequentially         | -       |
| `axe status`         | Show deployment progress                      | -       |
| `axe reset`          | Reset all steps to pending                    | -       |
| `axe test gmp`       | End-to-end GMP loopback test                  | no      |
| `axe test its`       | Deploy + transfer an interchain token         | no      |
| `axe test load-test` | Cross-chain load test                         | yes     |
| `axe decode`         | Decode EVM calldata (ITS, Gateway, Factory)   | -       |

## Deploy

```bash
axe deploy              # runs all 23 steps sequentially
axe status              # shows progress
axe reset               # start over
```

## Test GMP

```bash
axe test gmp
```

Sends a loopback GMP message and relays it through the full Amplifier pipeline end-to-end.

## Test ITS

```bash
axe test its
```

Deploys an interchain token locally, deploys it remotely to a destination chain via the ITS Hub, then sends a cross-chain transfer and verifies the balance on the destination. Relays through the full Amplifier pipeline (verify → vote → route → execute on hub).

# Load Tests
The axe test load-test command sends cross-chain transactions through the Axelar amplifier pipeline and verifies them end-to-end.

Sol → EVM: Derives N independent Solana keypairs (one per transaction), funds them, then sends all N CallContract transactions in parallel to avoid nonce contention. Each tx calls the Solana
Axelar Gateway with an ABI-encoded payload destined for a SenderReceiver contract on the EVM destination chain.

EVM → Sol: Sends N callContract transactions from a single EVM signer with a 200ms stagger between submissions to avoid RPC rate limits. Payloads target the Solana memo program.

Verification: After all transactions are submitted, a shared 3-minute timeout covers four sequential polling phases: voted (VotingVerifier quorum on Axelar), routed (Cosmos Gateway routing),
approved (destination chain gateway/PDA), and executed (approval consumed). Results are printed as a timing summary and saved to report.json.

## Load Test (SOL -> EVM)

```bash
axe test load-test \
  --source-chain solana-18 \
  --destination-chain avalanche-fuji \
  --config ../axelar-contract-deployments/axelar-chains-config/info/devnet-amplifier.json
```

## Load Test (EVM -> SOL)

```bash
axe test load-test \
  --source-chain avalanche-fuji \
  --destination-chain solana-18 \
  --config ../axelar-contract-deployments/axelar-chains-config/info/devnet-amplifier.json
```

Override the number of transactions (default: 5):

```bash
axe test load-test \
  --source-chain avalanche-fuji \
  --destination-chain solana-18 \
  --config ../axelar-contract-deployments/axelar-chains-config/info/devnet-amplifier.json \
  --num-txs 20
```

## Load Test (stagenet)

On stagenet/testnet/mainnet the relayer requires gas payment. Build with the appropriate feature flag:

```bash
cargo install --path . --no-default-features --features stagenet
axe test load-test \
  --source-chain solana-stagenet-3 \
  --destination-chain flow \
  --config ../axelar-contract-deployments/axelar-chains-config/info/stagenet.json \
  --num-txs 1
```

Run `axe test load-test --help` for all options.

## Decode

```bash
axe decode 0x0f4433d3...   # auto-detects function from 4-byte selector
axe decode 0x00000000...   # auto-detects ITS payload type
```

Decodes EVM calldata against a built-in ABI database (ITS Factory, ITS, Gateway). Recursively decodes nested bytes fields (multicall batches, ITS payloads inside GMP calls). Whitespace in hex input is stripped automatically.

## Configuration

All config lives in `.env` — see [`.env.example`](.env.example) for the full template.

| Variable                                                     | Used by                                    |
| ------------------------------------------------------------ | ------------------------------------------ |
| `CHAIN`, `ENV`, chain metadata                               | `init`                                     |
| `DEPLOYER_PRIVATE_KEY`, `GATEWAY_DEPLOYER_PRIVATE_KEY`, etc. | `deploy`                                   |
| `MNEMONIC`                                                   | `test gmp`, `test its` (Amplifier routing) |
| `ITS_*` vars                                                 | `deploy` (ITS steps), `test its`           |
| `TARGET_JSON`                                                | all commands (reads chain config)          |
