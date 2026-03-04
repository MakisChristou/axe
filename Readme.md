# axelar-evm-deployer

Deploy EVM chains to Axelar and run cross-chain load tests.

## Prerequisites

Clone `axelar-contract-deployments` as a sibling:

```bash
git clone https://github.com/axelarnetwork/axelar-contract-deployments.git
cd axelar-contract-deployments && npm install && cd ..
```

```
workspace/
├── axelar-evm-deployer/
└── axelar-contract-deployments/
```

## Deploy

```bash
cargo run -- deploy    # runs all 23 steps sequentially
cargo run -- status    # shows progress
```

Configure via `.env` — see [Configuration](#configuration) below.

## Load Test

```bash
cargo run --release -- test load-test \
  --config ../axelar-contract-deployments/axelar-chains-config/info/devnet-amplifier.json
```

Everything is auto-detected from the config: test type, source/destination chains, and RPCs. Defaults to 1 tx/sec for 10 seconds.

Verifies each message through 4 Amplifier checkpoints: Voted -> Routed -> Approved -> Executed.

| Type         | Direction    | Status      |
| ------------ | ------------ | ----------- |
| `sol-to-evm` | Solana -> EVM | supported   |
| `evm-to-sol` | EVM -> Solana | coming soon |
| `evm-to-evm` | EVM -> EVM   | coming soon |

Override anything:

```bash
cargo run --release -- test load-test \
  --config ../axelar-contract-deployments/axelar-chains-config/info/devnet-amplifier.json \
  --destination-chain avalanche-fuji \
  --source-chain solana-18 \
  --time 30 --delay 500
```

Run `cargo run -- test load-test --help` for all options.

## Test GMP

```bash
cargo run -- test gmp
```

Sends a loopback GMP message and relays it through the full Amplifier pipeline end-to-end.

---

## Configuration

Create a `.env` file (or export variables):

```bash
CHAIN=arc-11
SALT=v1.0.13
MNEMONIC="..."
MULTISIG_PROVER_MNEMONIC="..."
DEPLOYER_PRIVATE_KEY=0x...
GATEWAY_DEPLOYER_PRIVATE_KEY=0x...
GAS_SERVICE_DEPLOYER_PRIVATE_KEY=0x...

# ITS (optional)
ITS_DEPLOYER_PRIVATE_KEY=0x...
ITS_SALT=v2.2.0
ITS_PROXY_SALT=v1.0.0

# Environment
ENV=testnet

# Chain
CHAIN_NAME="Arc Testnet"
CHAIN_ID=5042002
RPC_URL=https://rpc.testnet.arc.network
TOKEN_SYMBOL=USDC
DECIMALS=18
EXPLORER_NAME="Arc Testnet Explorer"
EXPLORER_URL="https://testnet.arcscan.app/"
TARGET_JSON=../axelar-contract-deployments/axelar-chains-config/info/testnet.json
```

## Deploy Steps

All 23 steps run automatically. Governance proposals block until passed. Manual actions (voting, infra PRs) are prompted inline.

| #   | Step                         | Description                                                   |
| --- | ---------------------------- | ------------------------------------------------------------- |
| 1   | ConstAddressDeployer         | Deploy via CREATE                                             |
| 2   | Create3Deployer              | Deploy via CREATE2                                            |
| 3   | PredictGatewayAddress        | Predict gateway proxy address                                 |
| 4   | AddCosmWasmConfig            | Write VotingVerifier + MultisigProver config                  |
| 5-6 | InstantiateChainContracts   | Governance proposal + wait                                    |
| 7   | SaveDeployedContracts        | Query Coordinator for addresses                               |
| 8-9 | RegisterDeployment          | Governance proposal + wait                                    |
| 10-11 | CreateRewardPools          | Governance proposal + wait                                    |
| 12  | AddRewards                   | Fund reward pools                                             |
| 13  | WaitForVerifierSet           | Infra PR, poll verifiers, `update_verifier_set`               |
| 14  | AxelarGateway                | Deploy implementation + proxy                                 |
| 15-16 | Operators                  | Deploy + register                                             |
| 17  | AxelarGasService             | Deploy implementation + proxy                                 |
| 18-20 | Transfer Ownership         | Gateway, Operators, GasService -> governance                  |
| 21  | DeployInterchainTokenService | Deploy 9 ITS contracts via CREATE2/CREATE3                    |
| 22-23 | RegisterItsOnHub           | Governance proposal + wait                                    |
