# axe-tokens — per-network contract overlay

Deploy-once artifacts axe reuses across runs, for networks whose upstream
`axelar-chains-config` this repo cannot edit (and as a CI-visible complement
to the local caches, which fresh runners never have).

Schema mirrors chains-config: `chains.<axelarId>.contracts.<Name>`:

- `AXE.tokenId` / `AXE.address` — the canonical ITS test token for that chain
  (reused only when the running wallet still holds enough supply).
- `SenderReceiver.address` — the GMP helper contract; verified on-chain
  (code + gateway wiring) before reuse, so a stale entry falls back to a
  fresh deploy instead of breaking the run.

axe prints a 💡 hint with the exact entry to add whenever it deploys either
contract fresh.
