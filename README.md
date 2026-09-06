# Solana Snapshot Parser

Parses Solana snapshot data and generates `json` files for further pipeline processing.

The following CLI packages are used:

- **solana-parser-validator-cli**: Used within [.buildkite](./.buildkite/snapshot-fetch-and-parse.yml). 
  Parses the last slot snapshot of each epoch, generating information about validators and stakes.
- **solana-parser-tokens-cli**: Used within [Solana Snapshot Manager](https://github.com/marinade-finance/solana-snapshot-manager) to retrieve mSOL data,
  typically running once per day.

## Development notes

This project uses `solana-ledger` APIs that are part of the Agave Unstable API.
The `agave-unstable-api` feature is enabled on the `solana-ledger` crate in `Cargo.toml`.
These interfaces may change or break without warning in future Agave releases.

The validator meta collection records which cached vote-account state each
SIMD-0185 field was read from, and those vintages only hold for a bank frozen at
the last slot of an epoch. `snapshot-parser-validator-cli` refuses a snapshot
taken anywhere else rather than mislabel them, so the archive handed to
`--ledger-path` has to be the end-of-epoch one.
