use anchor_lang::prelude::*;
use anyhow::anyhow;
use solana_program::pubkey::Pubkey;
use std::cmp::min;

const SCALED_FACTOR_BASE: u64 = 1_000_000_000;

// imported from https://github.com/blockworks-foundation/voter-stake-registry/blob/release-v0.2.4/programs/voter-stake-registry/src/state/registrar.rs
#[derive(AnchorDeserialize)]
pub struct Registrar {
    pub discriminator: [u8; 8],
    pub governance_program_id: Pubkey,
    pub realm: Pubkey,
    pub realm_governing_token_mint: Pubkey,
    pub realm_authority: Pubkey,
    pub reserved1: [u8; 32],

    /// Storage for voting mints and their configuration.
    /// The length should be adjusted for one's use case.
    pub voting_mints: [VotingMintConfig; 4],

    /// Debug only: time offset, to allow tests to move forward in time.
    pub time_offset: i64,
    pub bump: u8,
    pub reserved2: [u8; 7],
    pub reserved3: [u64; 11], // split because `Default` does not support [u8; 95]
}

#[derive(AnchorDeserialize)]
pub struct VotingMintConfig {
    /// Mint for this entry.
    pub mint: Pubkey,

    /// The authority that is allowed to push grants into voters
    pub grant_authority: Pubkey,

    /// Vote weight factor for all funds in the account, no matter if locked or not.
    ///
    /// In 1/SCALED_FACTOR_BASE units.
    pub baseline_vote_weight_scaled_factor: u64,

    /// Maximum extra vote weight factor for lockups.
    ///
    /// This is the extra votes gained for lockups lasting lockup_saturation_secs or
    /// longer. Shorter lockups receive only a fraction of the maximum extra vote weight,
    /// based on lockup_time divided by lockup_saturation_secs.
    ///
    /// In 1/SCALED_FACTOR_BASE units.
    pub max_extra_lockup_vote_weight_scaled_factor: u64,

    /// Number of seconds of lockup needed to reach the maximum lockup bonus.
    pub lockup_saturation_secs: u64,

    /// Number of digits to shift native amounts, applying a 10^digit_shift factor.
    pub digit_shift: i8,

    // Empty bytes for future upgrades.
    pub reserved1: [u8; 7],
    pub reserved2: [u64; 7], // split because `Default` does not support [u8; 63]
}

impl VotingMintConfig {
    fn digit_shift_native(&self, amount_native: u64) -> anyhow::Result<u64> {
        let compute = || -> Option<u64> {
            let val = if self.digit_shift < 0 {
                (amount_native as u128).checked_div(10u128.pow((-self.digit_shift) as u32))?
            } else {
                (amount_native as u128).checked_mul(10u128.pow(self.digit_shift as u32))?
            };
            u64::try_from(val).ok()
        };
        compute().ok_or_else(|| anyhow!("VoterWeightOverflow"))
    }

    fn apply_factor(base: u64, factor: u64) -> anyhow::Result<u64> {
        let compute = || -> Option<u64> {
            u64::try_from(
                (base as u128)
                    .checked_mul(factor as u128)?
                    .checked_div(SCALED_FACTOR_BASE as u128)?,
            )
            .ok()
        };
        compute().ok_or_else(|| anyhow!("VoterWeightOverflow"))
    }

    pub fn baseline_vote_weight(&self, amount_native: u64) -> anyhow::Result<u64> {
        Self::apply_factor(
            self.digit_shift_native(amount_native)?,
            self.baseline_vote_weight_scaled_factor,
        )
    }
    pub fn max_extra_lockup_vote_weight(&self, amount_native: u64) -> anyhow::Result<u64> {
        Self::apply_factor(
            self.digit_shift_native(amount_native)?,
            self.max_extra_lockup_vote_weight_scaled_factor,
        )
    }
}

// imported from https://github.com/marinade-finance/voter-stake-registry/blob/governance-v3.1.0-marinade/programs/voter-stake-registry/src/state/voter.rs
#[derive(AnchorDeserialize)]
pub struct Voter {
    pub discriminator: [u8; 8],
    pub voter_authority: Pubkey,
    pub registrar: Pubkey,
    pub deposits: [DepositEntry; 32],
    pub voter_bump: u8,
    pub voter_weight_record_bump: u8,
    pub reserved: [u8; 94],
}

#[derive(AnchorDeserialize)]
pub struct DepositEntry {
    // Locked state.
    pub lockup: Lockup,

    /// Amount in deposited, in native currency. Withdraws of vested tokens
    /// directly reduce this amount.
    ///
    /// This directly tracks the total amount added by the user. They may
    /// never withdraw more than this amount.
    pub amount_deposited_native: u64,

    /// Amount in locked when the lockup began, in native currency.
    ///
    /// Note that this is not adjusted for withdraws. It is possible for this
    /// value to be bigger than amount_deposited_native after some vesting
    /// and withdrawals.
    ///
    /// This value is needed to compute the amount that vests each period,
    /// which should not change due to withdraws.
    pub amount_initially_locked_native: u64,

    // True if the deposit entry is being used.
    pub is_used: bool,

    /// If the clawback authority is allowed to extract locked tokens.
    pub allow_clawback: bool,

    // Points to the VotingMintConfig this deposit uses.
    pub voting_mint_config_idx: u8,

    pub reserved: [u8; 29],
}

impl DepositEntry {
    fn voting_power_linear_vesting(
        &self,
        curr_ts: i64,
        max_locked_vote_weight: u64,
        lockup_saturation_secs: u64,
    ) -> anyhow::Result<u64> {
        let periods_left = self.lockup.periods_left(curr_ts)?;
        let periods_total = self.lockup.periods_total()?;
        let period_secs = self.lockup.kind.period_secs();

        if periods_left == 0 {
            return Ok(0);
        }

        // This computes the voting power by considering the linear vesting as a
        // sequence of vesting cliffs.
        //
        // For example, if there were 5 vesting periods, with 3 of them left
        // (i.e. two have already vested and their tokens are no longer locked)
        // we'd have (max_locked_vote_weight / 5) weight in each of them, and the
        // voting power would be:
        //    (max_locked_vote_weight/5) * secs_left_for_cliff_1 / lockup_saturation_secs
        //  + (max_locked_vote_weight/5) * secs_left_for_cliff_2 / lockup_saturation_secs
        //  + (max_locked_vote_weight/5) * secs_left_for_cliff_3 / lockup_saturation_secs
        //
        // Or more simply:
        //    max_locked_vote_weight * (\sum_p secs_left_for_cliff_p) / (5 * lockup_saturation_secs)
        //  = max_locked_vote_weight * lockup_secs                    / denominator
        //
        // The value secs_left_for_cliff_p splits up as
        //    secs_left_for_cliff_p = min(
        //        secs_to_closest_cliff + (p-1) * period_secs,
        //        lockup_saturation_secs)
        //
        // If secs_to_closest_cliff < lockup_saturation_secs, we can split the sum
        //    \sum_p secs_left_for_cliff_p
        // into the part before saturation and the part after:
        // Let q be the largest integer 1 <= q <= periods_left where
        //        secs_to_closest_cliff + (q-1) * period_secs < lockup_saturation_secs
        //    =>  q = (lockup_saturation_secs - secs_to_closest_cliff + period_secs) / period_secs
        // and r be the integer where q + r = periods_left, then:
        //    lockup_secs := \sum_p secs_left_for_cliff_p
        //                 = \sum_{p<=q} secs_left_for_cliff_p
        //                   + r * lockup_saturation_secs
        //                 = q * secs_to_closest_cliff
        //                   + period_secs * \sum_0^q (p-1)
        //                   + r * lockup_saturation_secs
        //
        // Where the sum can be expanded to:
        //
        //    sum_full_periods := \sum_0^q (p-1)
        //                      = q * (q - 1) / 2
        //

        let secs_to_closest_cliff = self
            .lockup
            .seconds_left(curr_ts)
            .checked_sub(
                period_secs
                    .checked_mul(periods_left.saturating_sub(1))
                    .unwrap(),
            )
            .unwrap();

        if secs_to_closest_cliff >= lockup_saturation_secs {
            return Ok(max_locked_vote_weight);
        }

        // In the example above, periods_total was 5.
        let denominator = periods_total.checked_mul(lockup_saturation_secs).unwrap();

        let lockup_saturation_periods = lockup_saturation_secs
            .saturating_sub(secs_to_closest_cliff)
            .checked_add(period_secs)
            .unwrap()
            .checked_div(period_secs)
            .unwrap();
        let q = min(lockup_saturation_periods, periods_left);
        let r = periods_left.saturating_sub(q);

        // Sum of the full periods left for all remaining vesting cliffs.
        //
        // Examples:
        // - if there are 3 periods left, meaning three vesting cliffs in the future:
        //   one has only a fractional period left and contributes 0
        //   the next has one full period left
        //   and the next has two full periods left
        //   so sums to 3 = 3 * 2 / 2
        // - if there's only one period left, the sum is 0
        let sum_full_periods = q.checked_mul(q.saturating_sub(1)).unwrap() / 2;

        // Total number of seconds left over all periods_left remaining vesting cliffs
        let lockup_secs_fractional = q.checked_mul(secs_to_closest_cliff).unwrap();
        let lockup_secs_full = sum_full_periods.checked_mul(period_secs).unwrap();
        let lockup_secs_saturated = r.checked_mul(lockup_saturation_secs).unwrap();
        let lockup_secs = lockup_secs_fractional as u128
            + lockup_secs_full as u128
            + lockup_secs_saturated as u128;

        Ok(u64::try_from(
            (max_locked_vote_weight as u128)
                .checked_mul(lockup_secs)
                .unwrap()
                .checked_div(denominator as u128)
                .unwrap(),
        )?)
    }

    fn voting_power_cliff(
        &self,
        curr_ts: i64,
        max_locked_vote_weight: u64,
        lockup_saturation_secs: u64,
    ) -> anyhow::Result<u64> {
        let remaining = min(self.lockup.seconds_left(curr_ts), lockup_saturation_secs);
        Ok(u64::try_from(
            (max_locked_vote_weight as u128)
                .checked_mul(remaining as u128)
                .unwrap()
                .checked_div(lockup_saturation_secs as u128)
                .unwrap(),
        )?)
    }

    pub fn voting_power_locked(
        &self,
        curr_ts: i64,
        max_locked_vote_weight: u64,
        lockup_saturation_secs: u64,
    ) -> anyhow::Result<u64> {
        if self.lockup.expired(curr_ts) || max_locked_vote_weight == 0 {
            return Ok(0);
        }
        match self.lockup.kind {
            LockupKind::None => Ok(0),
            LockupKind::Daily => self.voting_power_linear_vesting(
                curr_ts,
                max_locked_vote_weight,
                lockup_saturation_secs,
            ),
            LockupKind::Monthly => self.voting_power_linear_vesting(
                curr_ts,
                max_locked_vote_weight,
                lockup_saturation_secs,
            ),
            LockupKind::Cliff => {
                self.voting_power_cliff(curr_ts, max_locked_vote_weight, lockup_saturation_secs)
            }
            LockupKind::Constant => {
                self.voting_power_cliff(curr_ts, max_locked_vote_weight, lockup_saturation_secs)
            }
        }
    }
    pub fn voting_power(
        &self,
        voting_mint_config: &VotingMintConfig,
        curr_ts: i64,
    ) -> anyhow::Result<u64> {
        let baseline_vote_weight =
            voting_mint_config.baseline_vote_weight(self.amount_deposited_native)?;
        let max_locked_vote_weight =
            voting_mint_config.max_extra_lockup_vote_weight(self.amount_initially_locked_native)?;
        let locked_vote_weight = self.voting_power_locked(
            curr_ts,
            max_locked_vote_weight,
            voting_mint_config.lockup_saturation_secs,
        )?;
        if max_locked_vote_weight < locked_vote_weight {
            return Err(anyhow::anyhow!(
                "assert_gte but max_locked_vote_weight {} is less than locked_vote_weight {}",
                max_locked_vote_weight,
                locked_vote_weight
            ));
        }
        baseline_vote_weight
            .checked_add(locked_vote_weight)
            .ok_or_else(|| anyhow::anyhow!("VoterWeightOverflow"))
    }
}

#[derive(AnchorDeserialize)]
pub struct Lockup {
    /// Start of the lockup.
    ///
    /// Note, that if start_ts is in the future, the funds are nevertheless
    /// locked up!
    ///
    /// Similarly, vote power computations don't care about start_ts and always
    /// assume the full interval from now to end_ts.
    pub(crate) start_ts: i64,

    /// End of the lockup.
    pub(crate) end_ts: i64,

    /// Type of lockup.
    pub kind: LockupKind,

    // Empty bytes for future upgrades.
    pub reserved: [u8; 15],
}

impl Lockup {
    pub fn expired(&self, curr_ts: i64) -> bool {
        self.seconds_left(curr_ts) == 0
    }

    pub fn seconds_left(&self, mut curr_ts: i64) -> u64 {
        if self.kind == LockupKind::Constant {
            curr_ts = self.start_ts;
        }
        if curr_ts >= self.end_ts {
            0
        } else {
            (self.end_ts - curr_ts) as u64
        }
    }

    pub fn periods_total(&self) -> anyhow::Result<u64> {
        let period_secs = self.kind.period_secs();
        if period_secs == 0 {
            return Ok(0);
        }

        let lockup_secs = self.seconds_left(self.start_ts);
        if !lockup_secs.is_multiple_of(period_secs) {
            return Err(anyhow!(
                "assert_eq but lockup_secs {} % period_secs {} != 0",
                lockup_secs,
                period_secs
            ));
        }

        Ok(lockup_secs.checked_div(period_secs).unwrap())
    }

    pub fn periods_left(&self, curr_ts: i64) -> anyhow::Result<u64> {
        let period_secs = self.kind.period_secs();
        if period_secs == 0 {
            return Ok(0);
        }
        if curr_ts < self.start_ts {
            return self.periods_total();
        }
        Ok(self
            .seconds_left(curr_ts)
            .checked_add(period_secs.saturating_sub(1))
            .unwrap()
            .checked_div(period_secs)
            .unwrap())
    }
}

#[repr(u8)]
#[derive(AnchorDeserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockupKind {
    /// No lockup, tokens can be withdrawn as long as not engaged in a proposal.
    None,

    /// Lock up for a number of days, where a linear fraction vests each day.
    Daily,

    /// Lock up for a number of months, where a linear fraction vests each month.
    Monthly,

    /// Lock up for a number of days, no vesting.
    Cliff,

    /// Lock up permanently. The number of days specified becomes the minimum
    /// unlock period when the deposit (or a part of it) is changed to Cliff.
    Constant,
}

pub const SECS_PER_DAY: u64 = 86_400;
pub const SECS_PER_MONTH: u64 = 365 * SECS_PER_DAY / 12;

impl LockupKind {
    /// The lockup length is specified by passing the number of lockup periods
    /// to create_deposit_entry. This describes a period's length.
    ///
    /// For vesting lockups, the period length is also the vesting period.
    pub fn period_secs(&self) -> u64 {
        match self {
            LockupKind::None => 0,
            LockupKind::Daily => SECS_PER_DAY,
            LockupKind::Monthly => SECS_PER_MONTH,
            LockupKind::Cliff => SECS_PER_DAY, // arbitrary choice
            LockupKind::Constant => SECS_PER_DAY, // arbitrary choice
        }
    }

    /// Lockups cannot decrease in strictness
    pub fn strictness(&self) -> u8 {
        match self {
            LockupKind::None => 0,
            LockupKind::Daily => 1,
            LockupKind::Monthly => 2,
            LockupKind::Cliff => 3, // can freely move between Cliff and Constant
            LockupKind::Constant => 3,
        }
    }

    pub fn is_vesting(&self) -> bool {
        match self {
            LockupKind::None => false,
            LockupKind::Daily => true,
            LockupKind::Monthly => true,
            LockupKind::Cliff => false,
            LockupKind::Constant => false,
        }
    }
}
