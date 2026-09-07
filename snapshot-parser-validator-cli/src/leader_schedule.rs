use {
    crate::validator_meta::{check_end_of_epoch_bank, VoteStateVintage},
    log::info,
    serde::{Deserialize, Serialize},
    snapshot_parser::serde_serialize::pubkey_string_conversion,
    solana_program::pubkey::Pubkey,
    solana_runtime::{bank::Bank, leader_schedule_utils::leader_schedule_from_vote_accounts},
    solana_stake_interface::stake_history::Epoch,
    solana_vote::vote_account::VoteAccountsHashMap,
    std::sync::Arc,
};

#[allow(deprecated)]
use solana_sdk::epoch_schedule::EpochSchedule;

// A flat row per slot, with the vintage repeated on each, because the stakes ETL loads
// this through `jq '.[]'` where nothing outside a row survives. See README for the
// column contract this shape owes that pipeline.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct LeaderScheduleEntry {
    pub epoch: Epoch,
    // absolute slot, not an index into the epoch
    pub slot: u64,
    #[serde(with = "pubkey_string_conversion")]
    pub vote_pubkey: Pubkey,
    // identity carried by the same vote account the schedule drew vote_pubkey from, so it
    // dates to vintage_captured_at_epoch and not to now
    #[serde(with = "pubkey_string_conversion")]
    pub node_pubkey: Pubkey,
    pub vintage_epoch_stakes_key: Epoch,
    pub vintage_captured_at_epoch: Epoch,
}

// deposit_or_burn_fee looks the resulting vote_address up in the same epoch_stakes(epoch)
// this draws from, so the schedule and the collector it feeds share one vintage
fn leader_schedule_entries(
    epoch: Epoch,
    epoch_schedule: &EpochSchedule,
    epoch_vote_accounts: &VoteAccountsHashMap,
) -> anyhow::Result<Vec<LeaderScheduleEntry>> {
    // LeaderSchedule::new panics rather than returning when nothing is staked
    if !epoch_vote_accounts
        .values()
        .any(|(stake, _vote_account)| *stake > 0)
    {
        anyhow::bail!("No staked vote account in epoch stakes for epoch {epoch}");
    }

    let leader_schedule =
        leader_schedule_from_vote_accounts(epoch, epoch_schedule, epoch_vote_accounts)
            .ok_or_else(|| anyhow::anyhow!("No leader schedule for epoch {epoch}"))?;
    let vintage = VoteStateVintage::from_epoch_stakes(epoch);
    let vintage_epoch_stakes_key = vintage.epoch_stakes_key.ok_or_else(|| {
        anyhow::anyhow!("Leader schedule vintage for epoch {epoch} names no epoch stakes snapshot")
    })?;
    let first_slot_in_epoch = epoch_schedule.get_first_slot_in_epoch(epoch);

    let entries: Vec<LeaderScheduleEntry> = leader_schedule
        .get_slot_leaders()
        .enumerate()
        .map(|(slot_index, slot_leader)| LeaderScheduleEntry {
            epoch,
            slot: first_slot_in_epoch.saturating_add(slot_index as u64),
            vote_pubkey: slot_leader.vote_address,
            node_pubkey: slot_leader.id,
            vintage_epoch_stakes_key,
            vintage_captured_at_epoch: vintage.captured_at_epoch,
        })
        .collect();

    // a slot count that is not a multiple of NUM_CONSECUTIVE_LEADER_SLOTS would leave
    // LeaderSchedule::new silently truncating the last leader window
    let slots_in_epoch = epoch_schedule.get_slots_in_epoch(epoch);
    if entries.len() as u64 != slots_in_epoch {
        anyhow::bail!(
            "Leader schedule for epoch {epoch} covers {} slots, not the {slots_in_epoch} slots the epoch has",
            entries.len()
        );
    }

    Ok(entries)
}

pub fn generate_leader_schedule(bank: &Arc<Bank>) -> anyhow::Result<Vec<LeaderScheduleEntry>> {
    assert!(bank.is_frozen());
    let epoch = bank.epoch();
    let epoch_schedule = bank.epoch_schedule();
    check_end_of_epoch_bank(
        bank.slot(),
        epoch_schedule.get_last_slot_in_epoch(epoch),
        epoch,
    )?;

    let epoch_vote_accounts = bank.epoch_vote_accounts(epoch).ok_or_else(|| {
        anyhow::anyhow!("No epoch stakes for epoch {epoch}; the bank cannot rebuild the leader schedule agave used")
    })?;
    let entries = leader_schedule_entries(epoch, epoch_schedule, epoch_vote_accounts)?;

    info!(
        "Leader schedule for epoch {}: {} slots drawn from {} vote accounts, vintage {:?}",
        epoch,
        entries.len(),
        epoch_vote_accounts.len(),
        VoteStateVintage::from_epoch_stakes(epoch)
    );

    Ok(entries)
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_sdk::account::AccountSharedData,
        solana_vote::vote_account::VoteAccount,
        solana_vote_interface::state::{VoteStateV3, VoteStateVersions},
        std::collections::{HashMap, HashSet},
    };

    const VOTE_PROGRAM_ID: &str = "Vote111111111111111111111111111111111111111";
    const EPOCH: Epoch = 1002;
    const VOTE_ACCOUNT: Pubkey = Pubkey::new_from_array([7u8; 32]);
    const NODE: Pubkey = Pubkey::new_from_array([17u8; 32]);
    const OTHER_VOTE_ACCOUNT: Pubkey = Pubkey::new_from_array([8u8; 32]);
    const OTHER_NODE: Pubkey = Pubkey::new_from_array([18u8; 32]);

    // Mainnet: 432,000 slots per epoch, warmup long over.
    fn mainnet_epoch_schedule() -> EpochSchedule {
        EpochSchedule::without_warmup()
    }

    fn vote_accounts<const N: usize>(entries: [(Pubkey, Pubkey, u64); N]) -> VoteAccountsHashMap {
        entries
            .into_iter()
            .map(|(vote_pubkey, node_pubkey, stake)| {
                let vote_state_versions = VoteStateVersions::new_v3(VoteStateV3 {
                    node_pubkey,
                    ..VoteStateV3::default()
                });
                let account = AccountSharedData::create_from_existing_shared_data(
                    1,
                    Arc::new(bincode::serialize(&vote_state_versions).unwrap()),
                    VOTE_PROGRAM_ID.parse().unwrap(),
                    false,
                    0,
                );
                (
                    vote_pubkey,
                    (stake, VoteAccount::try_from(account).unwrap()),
                )
            })
            .collect()
    }

    fn two_validators() -> VoteAccountsHashMap {
        vote_accounts([
            (VOTE_ACCOUNT, NODE, 1_000_000),
            (OTHER_VOTE_ACCOUNT, OTHER_NODE, 3_000_000),
        ])
    }

    #[test]
    fn the_schedule_covers_the_epoch_slot_range_with_no_gaps_and_no_duplicates() {
        let epoch_schedule = mainnet_epoch_schedule();
        let entries = leader_schedule_entries(EPOCH, &epoch_schedule, &two_validators()).unwrap();

        let slots: Vec<u64> = entries.iter().map(|entry| entry.slot).collect();
        let expected: Vec<u64> = (epoch_schedule.get_first_slot_in_epoch(EPOCH)
            ..=epoch_schedule.get_last_slot_in_epoch(EPOCH))
            .collect();

        assert_eq!(slots, expected);
        assert_eq!(
            slots.iter().collect::<HashSet<_>>().len(),
            slots.len(),
            "a slot must be claimed by exactly one row"
        );
        assert!(entries.iter().all(|entry| entry.epoch == EPOCH));
    }

    #[test]
    fn the_slot_count_equals_the_epoch_schedule_slots_in_epoch() {
        let epoch_schedule = mainnet_epoch_schedule();

        let entries = leader_schedule_entries(EPOCH, &epoch_schedule, &two_validators()).unwrap();

        assert_eq!(
            entries.len() as u64,
            epoch_schedule.get_slots_in_epoch(EPOCH)
        );
        assert_eq!(entries.len(), 432_000);
    }

    // get_slots_in_epoch is not constant while the schedule is warming up, so a
    // hard-coded slots_per_epoch would be wrong for every early epoch.
    #[test]
    fn a_warming_up_epoch_schedule_gets_its_own_shorter_slot_count() {
        let epoch_schedule = EpochSchedule::new(432_000);
        assert!(epoch_schedule.warmup);

        for epoch in 0..4 {
            let slots_in_epoch = epoch_schedule.get_slots_in_epoch(epoch);
            assert_eq!(
                slots_in_epoch,
                32 << epoch,
                "warmup epoch {epoch} is not the normal length"
            );

            let entries =
                leader_schedule_entries(epoch, &epoch_schedule, &two_validators()).unwrap();

            assert_eq!(entries.len() as u64, slots_in_epoch);
            assert_eq!(
                entries.first().unwrap().slot,
                epoch_schedule.get_first_slot_in_epoch(epoch)
            );
            assert_eq!(
                entries.last().unwrap().slot,
                epoch_schedule.get_last_slot_in_epoch(epoch)
            );
        }
    }

    #[test]
    fn a_slot_gets_the_vote_account_and_node_identity_of_one_vote_account() {
        let entries =
            leader_schedule_entries(EPOCH, &mainnet_epoch_schedule(), &two_validators()).unwrap();

        let authoritative_pairs =
            HashMap::from([(VOTE_ACCOUNT, NODE), (OTHER_VOTE_ACCOUNT, OTHER_NODE)]);
        for entry in &entries {
            assert_eq!(
                authoritative_pairs.get(&entry.vote_pubkey),
                Some(&entry.node_pubkey),
                "slot {} pairs a vote account with an identity no vote account carries",
                entry.slot
            );
        }
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.vote_pubkey)
                .collect::<HashSet<_>>()
                .len(),
            2,
            "both staked vote accounts have to lead some slot at these weights"
        );
    }

    // Two vote accounts sharing one identity is what SIMD-0180 exists for, and
    // the reason an identity-keyed schedule cannot be inverted downstream.
    #[test]
    fn two_vote_accounts_on_one_identity_stay_distinguishable_by_vote_pubkey() {
        let entries = leader_schedule_entries(
            EPOCH,
            &mainnet_epoch_schedule(),
            &vote_accounts([
                (VOTE_ACCOUNT, NODE, 1_000_000),
                (OTHER_VOTE_ACCOUNT, NODE, 3_000_000),
            ]),
        )
        .unwrap();

        assert!(entries.iter().all(|entry| entry.node_pubkey == NODE));
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.vote_pubkey)
                .collect::<HashSet<_>>()
                .len(),
            2
        );
    }

    #[test]
    fn epoch_stakes_with_nothing_staked_is_an_error_rather_than_a_panic() {
        let err = leader_schedule_entries(
            EPOCH,
            &mainnet_epoch_schedule(),
            &vote_accounts([(VOTE_ACCOUNT, NODE, 0)]),
        )
        .expect_err("LeaderSchedule::new would panic on this input");

        assert!(
            err.to_string().contains("No staked vote account"),
            "the error must say what the snapshot is missing: {err}"
        );
    }

    #[test]
    fn an_unstaked_vote_account_leads_no_slot() {
        let entries = leader_schedule_entries(
            EPOCH,
            &mainnet_epoch_schedule(),
            &vote_accounts([
                (VOTE_ACCOUNT, NODE, 1_000_000),
                (OTHER_VOTE_ACCOUNT, OTHER_NODE, 0),
            ]),
        )
        .unwrap();

        assert!(entries
            .iter()
            .all(|entry| entry.vote_pubkey == VOTE_ACCOUNT));
    }

    // The schedule is built from epoch_stakes(epoch), the state captured at the
    // first slot of the epoch before it; the file must say so per row.
    #[test]
    fn every_row_records_the_epoch_stakes_snapshot_the_schedule_was_built_from() {
        let entries =
            leader_schedule_entries(EPOCH, &mainnet_epoch_schedule(), &two_validators()).unwrap();

        assert!(entries
            .iter()
            .all(|entry| entry.vintage_epoch_stakes_key == EPOCH
                && entry.vintage_captured_at_epoch == EPOCH - 1));

        let vintage = VoteStateVintage::from_epoch_stakes(EPOCH);
        assert_eq!(
            (
                Some(entries.first().unwrap().vintage_epoch_stakes_key),
                entries.first().unwrap().vintage_captured_at_epoch
            ),
            (vintage.epoch_stakes_key, vintage.captured_at_epoch),
            "the vintage has to be the one validator_meta records for the same snapshot"
        );
    }

    // The stakes ETL loads this file with `jq '.[]' -rc` straight into BigQuery,
    // so a renamed field has to break the build here rather than a load
    #[test]
    fn the_leader_schedule_entry_json_shape_is_pinned() {
        let entry = LeaderScheduleEntry {
            epoch: 1002,
            slot: 433_295_999,
            vote_pubkey: VOTE_ACCOUNT,
            node_pubkey: NODE,
            vintage_epoch_stakes_key: 1002,
            vintage_captured_at_epoch: 1001,
        };

        assert_eq!(
            serde_json::to_value(&entry).unwrap(),
            serde_json::json!({
                "epoch": 1002,
                "slot": 433_295_999u64,
                "vote_pubkey": "US517G5965aydkZ46HS38QLi7UQiSojurfbQfKCELFx",
                "node_pubkey": "29d2S7vB453rNYFdR5Ycwt7y9haRT5fwVwL9zTmBhfV2",
                "vintage_epoch_stakes_key": 1002,
                "vintage_captured_at_epoch": 1001,
            })
        );
        assert_eq!(
            entry,
            serde_json::from_str(&serde_json::to_string(&entry).unwrap()).unwrap()
        );
    }

    // The stakes ETL runs `jq '.[]' -rc` over the file and feeds the result to
    // `bq load --source_format=NEWLINE_DELIMITED_JSON`, so the document has to be
    // one JSON array of flat objects: a collection-level header field or a
    // grouped body would not survive that transform.
    #[test]
    fn the_document_is_one_json_array_of_flat_objects() {
        let entries =
            leader_schedule_entries(EPOCH, &mainnet_epoch_schedule(), &two_validators()).unwrap();
        let document = serde_json::to_string(&entries).unwrap();

        assert!(
            !document.contains('\n'),
            "the compact writer must emit no newlines"
        );
        let rows: Vec<serde_json::Value> = serde_json::from_str(&document).unwrap();
        assert_eq!(rows.len(), entries.len());
        assert!(rows.iter().all(|row| row
            .as_object()
            .is_some_and(|row| row["vote_pubkey"].is_string() && row["slot"].is_u64())));
        // sorted, because serde_json keeps object keys in whatever order the
        // preserve_order feature of the build leaves them in
        let columns = [
            "epoch",
            "node_pubkey",
            "slot",
            "vintage_captured_at_epoch",
            "vintage_epoch_stakes_key",
            "vote_pubkey",
        ];
        for row in &rows {
            let mut keys: Vec<&str> = row
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            keys.sort_unstable();
            assert_eq!(
                keys, columns,
                "a row is exactly the six columns the stakes ETL loads, with no nested object among them"
            );
        }
    }
}
