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
SIMD-0185 field was read from, and those vintages hold for any bank frozen inside
the epoch it reports: `epoch_stakes(E)` and `epoch_stakes(E+1)` are created before
epoch E starts and agave keeps both for its whole length.
`snapshot-parser-validator-cli` therefore refuses only an archive that has crossed
into the next epoch, and warns when the bank is short of the epoch's last slot -
the leader of that slot may simply have skipped it, leaving no bank there to parse.
A bank short of the boundary reads `credits` and the collector vintage that many
slots early, and publishes the `slot` it read them at.

### Which vintage each `validators.json` field is read at

agave keys `Bank::epoch_stakes` by leader schedule epoch, so `epoch_stakes(E)`
holds vote state as captured at the first slot of `E - 1`. For a bank frozen at
the last slot of epoch E the parser reads each field from the snapshot agave
itself reads it from, and publishes the vintage beside it:

| field | source | agave |
|---|---|---|
| `inflation_rewards_collector`, `pending_delegator_rewards` | live stakes cache at `slot`, published as `collector_vintage` | `distribution_epoch_vote_accounts`, which is `epoch_stakes(E+2)` as the distribution bank snapshots it |
| `inflation_rewards_commission_bps` | `epoch_stakes(E)`, falling back to `epoch_stakes(E+1)` then to the state at `slot`, published as `commission_vintage` | `snapshot_epoch_vote_accounts.or_else(rewarded_epoch_vote_accounts)` in `get_cached_vote_accounts` |
| `block_revenue_collector`, `block_revenue_commission_bps` | `epoch_stakes(E)` only, no fallback | `deposit_or_burn_fee`, which resolves the leader there and expects it to be present |

Two counters say how many vote accounts missed the commission vintage:
`commission_vintage_next_snapshot_fallbacks` were answered by
`epoch_stakes(E+1)`, `commission_vintage_live_state_fallbacks` by neither
snapshot. Both are 0 when `commission_vintage` is the live stakes cache, since
then no snapshot was named to fall back from. Both vintages are `null` in a
`validators.json` written before they were published - absence means the file
recorded none, not that the live stakes cache was used.

A third counter, `leader_schedule_vote_accounts_absent_at_slot`, is the one thing
that ties the two output files together. The leader schedule is drawn from
`epoch_stakes(E)` while `validator_metas` is built from the live stakes cache at
`slot`, so a vote account that was staked when the schedule was fixed but closed
before the epoch ended leads slots that no row here can answer for. It is normally
0; when it is not, a `leader_schedule.vote_pubkey -> validators.vote_account` join
has that many vote accounts with no right-hand side, and the parser logs a warning
naming the count.

The collector vintage is the one place the parser is knowingly wider than agave:
`bank.vote_accounts()` is unfiltered, while agave pays only the vote accounts
that survive SIMD-0357 admission filtering, so a filtered-out account still gets
a row here and earns nothing. `features.inflation_rewards_validator_admission_ticket_active`
says whether that filter is in play; the top-N part of it is not something an
end-of-E bank can reproduce.

`features` publishes the agave flags these vintages depend on. The two block
revenue flags are independent and neither stands in for the other:
`block_revenue_custom_collector_active` (SIMD-0232) decides whether the
collector is honoured at all, and `block_revenue_sharing_active` (SIMD-0123)
decides whether `block_revenue_commission_bps` splits anything. The inflation
flags are `Option<bool>`, reported as `Some(true)` once active and `None` while
not: agave calculates epoch E's rewards on the first bank of E+1 after that bank
has applied its own activations, so this bank can prove a flag active but never
prove one inactive.

`--output-leader-schedule` is optional and off by default. When it is given, the
parser rebuilds the epoch's leader schedule from `epoch_stakes(epoch)` and writes
one row per slot, keyed by vote account as SIMD-0180 keys it, with the node
identity from the same vote account. That is 432,000 rows for a mainnet epoch, so
the file is written as compact JSON: still a single JSON array of flat objects,
which is what `jq '.[]'` and `bq load --source_format=NEWLINE_DELIMITED_JSON`
consume, just without the indentation.

A failed leader schedule is not fatal unless `--require-leader-schedule true` says
so, in the same way `--require-jito-stake-meta` governs the Jito collection. The
mainnet pipeline passes `false` explicitly, so the epoch still publishes the three
collections the stakes ETL cannot do without and the exit code alone says whether
anything required failed.

Keying by vote account rather than identity is what SIMD-0232 requires: once a
`Fee` reward row is credited to a `block_revenue_collector` the validator may
point anywhere, the row stops naming the leader, and only the schedule does. It
is not a fix for identity multiplicity as things stand - mainnet on 2026-09-07
held 6853 vote accounts over 6467 identities, 247 of them with more than one
vote account, but **zero** with more than one *staked* vote account, and only
staked accounts enter a schedule. The 9 leaders whose identity carries an extra
unstaked vote account are ambiguous only to a resolver that also sees unstaked
accounts, which is what makes an identity-keyed join fragile rather than already
wrong.

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
