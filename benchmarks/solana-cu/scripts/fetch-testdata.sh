#!/usr/bin/env bash
# Fetch the mainnet Solana program binaries the CU harness runs under LiteSVM.
#
# The harness measures compute units against the REAL deployed bytecode, so it
# loads each program's mainnet .so at that program's id. These are not committed
# (they are ~2 MB and go stale as mainnet redeploys); run this once, or again
# after a redeploy. `axe bench solana-cu` runs it automatically when the files
# are missing.
#
# Requires the `solana` CLI and network access. Override the RPC with
# SOLANA_RPC_URL (defaults to the public mainnet-beta endpoint).
set -euo pipefail

DIR="$(cd "$(dirname "$0")/.." && pwd)/tests/testdata"
URL="${SOLANA_RPC_URL:-https://api.mainnet-beta.solana.com}"
mkdir -p "$DIR"

# program id -> output filename. IDs are the mainnet deployments
# (axelar-contract-deployments mainnet.json / axelar-app native-unwrapper). The
# harness loads each by its crate ::ID, so a wrong id here fails the test loudly.
dump() {
  echo "  $1 -> $2"
  solana program dump "$1" "$DIR/$2" --url "$URL"
}

echo "Dumping mainnet Solana programs into $DIR (RPC: $URL)"
dump itsAUdHnV2K99ppbM6d6WUDac8MD54RAE7dUKHnw1Eg its.so
dump gtwqvLL93XK7pC2eMvfGamqokvs19AytzaVhrL2iKiz gateway.so
dump gaszjG8797GGm8oACCzH2KLLifGp2nugKkLwaecwBjT gas_service.so
dump unw1CzbeMFnmPH4fAYfNqCCZwBsWYPEGLeDtmaRsXEq native_unwrapper.so
# Metaplex Token Metadata: the ITS deployment path CPIs into it to create the
# new mint's metadata account.
dump metaqbxxUerdq28cj1RbAWkYQm3ybzjb6a8bt518x1s mpl_token_metadata.so
echo "Done."
