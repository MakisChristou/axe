# Useful Queries (devnet-amplifier)

## Query poll status on the Voting Verifier

`PollId` (Uint64) must be passed as a **string**, not a number.

```bash
axelard q wasm contract-state smart <voting_verifier_address> '{"poll":{"poll_id":"<POLL_ID>"}}' --node http://devnet-amplifier.axelar.dev:26657
```

Example (solana-18, poll 46):
```bash
axelard q wasm contract-state smart axelar1kvc52ststqy0fr5thuuvqlwhjcrt78ccl8ts6e9vzue6xu6wgqjsklrd4k '{"poll":{"poll_id":"46"}}' --node http://devnet-amplifier.axelar.dev:26657
```

## Other VotingVerifier queries
- `{"poll": {"poll_id": "<id>"}}` — poll response (tallies, status, messages)
- `{"messages_status": [<Message>, ...]}` — verification status of messages
- `{"voting_parameters": {}}` — current voting threshold, block_expiry, confirmation_height

## Endpoints
- LCD: `http://devnet-amplifier.axelar.dev:1317`
- RPC: `http://devnet-amplifier.axelar.dev:26657`

## Contract addresses (solana-18)
- Voting Verifier: `axelar1kvc52ststqy0fr5thuuvqlwhjcrt78ccl8ts6e9vzue6xu6wgqjsklrd4k`
- Source Gateway: `gtwT4uGVTYSPnTGv6rSpMheyFyczUicxVWKqdtxNGw9`

## Query via LCD REST (no axelard needed)
```bash
echo -n '{"poll":{"poll_id":"46"}}' | base64 | xargs -I{} curl -s "http://devnet-amplifier.axelar.dev:1317/cosmwasm/wasm/v1/contract/axelar1kvc52ststqy0fr5thuuvqlwhjcrt78ccl8ts6e9vzue6xu6wgqjsklrd4k/smart/{}" | jq
```
