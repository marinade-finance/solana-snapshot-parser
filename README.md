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

### The leader schedule contract with the stakes ETL

The file is named `leader-schedule.json`, and the pipeline that runs the parser
for an epoch has to publish it as `<snapshot bucket>/<epoch>/leader-schedule.json` -
`$GCLOUD_SNAPSHOTS/$epoch/` here, `$GS_SNAPSHOT_BUCKET/<epoch>/` on the consuming
side, one bucket under two names - next to the `stakes.json` and
`validators.json` it already publishes there. Publishing is that pipeline's job;
the parser only writes the path it is given, so `--output-leader-schedule` has to
name that file. The
[stakes ETL](https://github.com/marinade-finance/stakes-etl) fetches exactly that
object, loads it into `mainnet_beta_stakes.leader_schedule` with
`jq '.[]' -rc | bq load --source_format=NEWLINE_DELIMITED_JSON`, and refuses a
file that carries anything else.

The document is one compact JSON array of flat objects, each of them exactly
these six keys and no others:

| key | type | BigQuery |
|---|---|---|
| `epoch` | integer | `INT64` |
| `slot` | integer | `INT64` |
| `vote_pubkey` | base58 string | `STRING(44)` |
| `node_pubkey` | base58 string | `STRING(44)` |
| `vintage_epoch_stakes_key` | integer | `INT64` |
| `vintage_captured_at_epoch` | integer | `INT64` |

The two vintage keys are `VoteStateVintage` flattened out per row, not a nested
object: the load has one column per key, and nothing outside a row survives
`jq '.[]'`, so a collection-level header would never reach BigQuery.
`vintage_epoch_stakes_key` is the `Bank::epoch_stakes` snapshot the schedule was
drawn from, which for the schedule of epoch E is E itself - the stakes ETL
refuses a schedule of epoch E whose rows say anything else, since that file
would re-attribute a whole epoch of block fees to another epoch's leaders - and
`vintage_captured_at_epoch` is the epoch at whose first slot the vote states in
that snapshot were captured, i.e. E-1. A renamed or nested key breaks the shape
test in `leader_schedule.rs` rather than a production `bq load`.
