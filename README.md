# Solana Snapshot Parser

Parses Solana snapshot data and generates `json` files for further pipeline processing.

The following CLI packages are used:

- **solana-parser-validator-cli**: Used within [.buildkite](./.buildkite/snapshot-fetch-and-parse.yml). 
  Parses the last slot snapshot of each epoch, generating information about validators and stakes.
- **solana-parser-tokens-cli**: Used within [Solana Snapshot Manager](https://github.com/marinade-finance/solana-snapshot-manager) to retrieve mSOL data,
  typically running once per day.

## Development notes

Using a deprecated field `solana_ledger::*`. This crate has been marked for formal inclusion
in the Agave Unstable API. From v4.0.0 onward, the `agave-unstable-api` crate feature must
be specified to acknowledge use of an interface that may break without warning.
