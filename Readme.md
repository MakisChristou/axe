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

Override anything:

```bash
axe test load-test \
  --config ../axelar-contract-deployments/axelar-chains-config/info/devnet-amplifier.json \
  --destination-chain avalanche-fuji \
  --source-chain solana-18 \
  --num-keys 20 --time 30 --delay 500
```

Run `axe test load-test --help` for all options.

## Configuration

All config lives in `.env` — see [`.env.example`](.env.example) for the full template.

| Variable                                                     | Used by                                    |
| ------------------------------------------------------------ | ------------------------------------------ |
| `CHAIN`, `ENV`, chain metadata                               | `init`                                     |
| `DEPLOYER_PRIVATE_KEY`, `GATEWAY_DEPLOYER_PRIVATE_KEY`, etc. | `deploy`                                   |
| `MNEMONIC`                                                   | `test gmp`, `test its` (Amplifier routing) |
| `ITS_*` vars                                                 | `deploy` (ITS steps), `test its`           |
| `TARGET_JSON`                                                | all commands (reads chain config)          |
