# Solana Snapshot Parser

Parses Solana snapshot data and generates `json` files for further pipeline processing.

The following CLI packages are used:

- **solana-parser-validator-cli**: Used within [.buildkite](./.buildkite/snapshot-fetch-and-parse.yml). 
  Parses the last slot snapshot of each epoch, generating information about validators and stakes.
- **solana-parser-tokens-cli**: Used within [Solana Snapshot Manager](https://github.com/marinade-finance/solana-snapshot-manager) to retrieve mSOL data,
  typically running once per day.
- **snapshot-parser-types**: A type library used in the [Validator Bonds](https://github.com/marinade-finance/validator-bonds) project.
  It is intentionally kept outside the workspace due to dependency conflicts.  
  You can build it separately with:
  ```sh
  cargo build --manifest-path=snapshot-parser-types/Cargo.toml
  ```
