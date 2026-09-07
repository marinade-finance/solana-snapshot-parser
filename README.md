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

`--output-leader-schedule` is optional and off by default. When it is given, the
parser rebuilds the epoch's leader schedule from `epoch_stakes(epoch)` and writes
one row per slot, keyed by vote account as SIMD-0180 keys it, with the node
identity from the same vote account. That is 432,000 rows for a mainnet epoch, so
the file is written as compact JSON: still a single JSON array of flat objects,
which is what `jq '.[]'` and `bq load --source_format=NEWLINE_DELIMITED_JSON`
consume, just without the indentation.
