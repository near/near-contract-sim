//! The stake each epoch locks, and what an epoch boundary hands back, as
//! nearcore's epoch manager computes it for the block that starts an epoch.
//!
//! A `Stake` action locks a higher stake at once; a lower one only proposes
//! it (`action_stake`). Proposals count in the next block's epoch, the last
//! per account winning. When epoch `e` ends, its proposals, and `e + 1`'s
//! validators that made none, become `e + 2`'s stake
//! (`apply_epoch_update_to_proposals`). At `e + 1`'s first block an account
//! keeps locked the most it stakes in `e`..=`e + 2`
//! (`compute_stake_return_info`) and the runtime returns the rest
//! (`update_validator_accounts`): an unstake counted in `e` frees at
//! `e + 3`'s first block. Every positive proposal is accepted (no seat
//! price, no kickouts); the runtime's validator is still only
//! `validator.near`.
//!
//! A direct write (`apply_state_records`, a genesis, `sandbox_patch_state`)
//! giving an account `locked` where it had none seeds it (`seed`).
//! A write that changes a nonzero `locked` leaves the stakes alone, as the
//! epoch manager would: lowered, the next boundary fails nearcore's FATAL
//! staking check; raised, it hands the excess back. Seeded stakes live in
//! the log, so clones, `Sim::cached` snapshots and rewinds keep them.

use borsh::{BorshDeserialize, BorshSerialize};
use near_primitives::types::validator_stake::ValidatorStake;
use near_primitives::types::{AccountId, Balance};
use node_runtime::ValidatorAccountsUpdate;
use std::collections::{BTreeMap, BTreeSet};

type Stakes = BTreeMap<AccountId, Balance>;

#[derive(Clone, BorshSerialize, BorshDeserialize)]
pub(crate) struct Epochs {
    /// The epoch the last boundary entered; a rewind never lowers it.
    epoch: u64,
    /// Each epoch's stake (`stake_change`), at 0 an account that just left.
    stakes: BTreeMap<u64, Stakes>,
    /// Proposals by the epoch they count in.
    proposals: BTreeMap<u64, Stakes>,
    /// The last block's proposals: the next chunk's `last_proposals`.
    last: Stakes,
    /// The `epoch_length` every epoch above is counted in.
    length: Option<u64>,
}

impl Default for Epochs {
    fn default() -> Self {
        Epochs {
            epoch: 1,
            stakes: BTreeMap::new(),
            proposals: BTreeMap::new(),
            last: Stakes::new(),
            length: None,
        }
    }
}

impl Epochs {
    /// The update the block of epoch `to` applies with, if some account has
    /// stake to keep or get back; each staked account earns `reward` an
    /// epoch. Several boundaries skipped make one update, rewards summed:
    /// no proposal comes between them, so what stays locked only shrinks.
    /// Steady or empty stretches are added in one step.
    pub(crate) fn enter(
        &mut self,
        to: u64,
        length: u64,
        reward: Balance,
    ) -> Option<ValidatorAccountsUpdate> {
        self.length.get_or_insert(length);
        let (mut accounts, mut rewards) = (BTreeSet::new(), Stakes::new());
        while self.epoch < to {
            // Nothing staked nor proposed: every boundary up to `to` is alike.
            if self.idle() {
                self.epoch = to;
                break;
            }
            let e = self.epoch;
            if self.proposals.is_empty() && self.steady(e, reward) {
                let k = u128::from(to - e);
                let earned = reward.checked_mul(k).expect("reward overflow");
                let shift = |e: u64| -> Stakes {
                    self.stakes[&e].iter().map(|(a, s)| (a.clone(), add(*s, earned))).collect()
                };
                let [now, next] = [shift(e), shift(e + 1)];
                for a in next.keys() {
                    credit(&mut rewards, a, earned);
                    accounts.insert(a.clone());
                }
                self.stakes.insert(to, now);
                self.stakes.insert(to + 1, next);
                self.epoch = to;
                break;
            }
            let [now, next] = [e, e + 1].map(|e| self.stakes.get(&e).cloned().unwrap_or_default());
            let earned = |a: &AccountId| {
                if now.get(a).is_some_and(|s| !s.is_zero()) { reward } else { Balance::ZERO }
            };
            // `e + 1`'s validators stay with what they earned; a proposal
            // replaces a stake. Proposing 0 leaves for good (`Unstaked`).
            let mut after: Stakes = next
                .iter()
                .filter(|(_, s)| !s.is_zero())
                .map(|(a, s)| (a.clone(), add(*s, earned(a))))
                .collect();
            for (a, p) in self.proposals.remove(&e).unwrap_or_default() {
                let stays = !p.is_zero() && after.contains_key(&a);
                let stake = if stays { add(p, earned(&a)) } else { p };
                after.insert(a, stake);
            }
            for a in now.keys() {
                credit(&mut rewards, a, earned(a));
            }
            accounts.extend(now.into_keys().chain(next.into_keys()).chain(after.keys().cloned()));
            self.stakes.insert(e + 2, after);
            self.epoch += 1;
        }
        if accounts.is_empty() {
            return None;
        }
        let most = |a: &AccountId| {
            let stakes = [to - 1, to, to + 1].map(|e| self.stakes.get(&e).and_then(|s| s.get(a)));
            stakes.into_iter().flatten().copied().max().unwrap_or_default()
        };
        let stake_info = accounts.iter().map(|a| (a.clone(), most(a))).collect();
        self.stakes.retain(|&e, _| e >= to);
        Some(ValidatorAccountsUpdate {
            stake_info,
            validator_rewards: rewards.into_iter().collect(),
            last_proposals: self.last.clone().into_iter().collect(),
            protocol_treasury_account_id: None,
        })
    }

    /// Panics if the chain's epochs were counted in another length.
    #[track_caller]
    pub(crate) fn check_length(&self, length: u64) {
        if let Some(first) = self.length {
            assert!(
                first == length,
                "config.epoch_length is {length}, but the chain's epochs were counted in \
                 {first} blocks: set it before the first block"
            );
        }
    }

    /// Nothing staked from the current epoch on, and nothing proposed.
    fn idle(&self) -> bool {
        self.stakes.range(self.epoch..).all(|(_, s)| s.is_empty()) && self.proposals.is_empty()
    }

    /// Every stake of `e` is `reward` more in `e + 1`: each boundary from
    /// `e` on adds `reward` to every stake.
    fn steady(&self, e: u64, reward: Balance) -> bool {
        let (Some(now), Some(next)) = (self.stakes.get(&e), self.stakes.get(&(e + 1))) else {
            return false;
        };
        !now.is_empty()
            && now.len() == next.len()
            && now.iter().all(|(a, s)| !s.is_zero() && next.get(a) == Some(&add(*s, reward)))
    }

    /// Counts `account`, written `locked` where it had none, as staked in
    /// the current epoch and the next, as a genesis validator is: it earns
    /// from then on, and an unstake waits the full delay. With nothing
    /// staked, the epoch first moves up to `now` (no pay for skipped
    /// epochs). A no-op for an account still staked or proposing.
    pub(crate) fn seed(&mut self, account: &AccountId, locked: Balance, now: u64) {
        let staked = |s: &Stakes| s.get(account).is_some_and(|s| !s.is_zero());
        if self.stakes.range(self.epoch..).any(|(_, s)| staked(s))
            || self.proposals.values().any(|p| p.contains_key(account))
        {
            return;
        }
        if self.idle() {
            self.epoch = self.epoch.max(now);
        }
        for e in [self.epoch, self.epoch + 1] {
            self.stakes.entry(e).or_default().insert(account.clone(), locked);
        }
    }

    /// Entering `to` passes a boundary with a stake and no reward to pay.
    pub(crate) fn unrewarded(&self, to: u64, reward: Balance) -> bool {
        let staked = |s: &Stakes| s.values().any(|b| !b.is_zero());
        reward.is_zero() && self.epoch < to && self.stakes.get(&self.epoch).is_some_and(staked)
    }

    /// Counts a block's proposals in epoch `at` (the current one after a
    /// rewind); the runtime keeps each account's last.
    pub(crate) fn propose(&mut self, at: u64, proposals: &[ValidatorStake]) {
        self.last = proposals.iter().map(|p| (p.account_id().clone(), p.stake())).collect();
        if !self.last.is_empty() {
            self.proposals.entry(at.max(self.epoch)).or_default().extend(self.last.clone());
        }
    }
}

#[track_caller]
fn add(a: Balance, b: Balance) -> Balance {
    a.checked_add(b).expect("stake overflow")
}

fn credit(rewards: &mut Stakes, a: &AccountId, amount: Balance) {
    let r = rewards.entry(a.clone()).or_default();
    *r = add(*r, amount);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shortcut's condition: every account staked in both epochs, each
    /// up by exactly the reward, and some account staked.
    #[test]
    fn steady_needs_every_stake_up_by_the_reward() {
        let stakes = |pairs: &[(&str, u128)]| -> Stakes {
            let stake = |&(a, s): &(&str, u128)| (a.parse().unwrap(), Balance::from_yoctonear(s));
            pairs.iter().map(stake).collect()
        };
        let steady = |now: &[(&str, u128)], next: &[(&str, u128)]| {
            let mut e = Epochs::default();
            e.stakes.insert(1, stakes(now));
            e.stakes.insert(2, stakes(next));
            e.steady(1, Balance::from_yoctonear(1))
        };
        assert!(steady(&[("a.near", 5)], &[("a.near", 6)]));
        assert!(!steady(&[("a.near", 5)], &[("a.near", 5)]), "not up by the reward");
        assert!(!steady(&[("a.near", 5)], &[("a.near", 6), ("b.near", 1)]), "a newcomer in e + 1");
        assert!(!steady(&[("a.near", 0)], &[("a.near", 1)]), "a zero stake");
        assert!(!steady(&[], &[]), "nothing staked");
        assert!(!Epochs::default().steady(1, Balance::from_yoctonear(1)), "no stakes at all");
    }

    /// An epoch boundary passed with stake and a zero reward is noted;
    /// with no stake, a reward, or no boundary, it isn't.
    #[test]
    fn a_boundary_with_stake_and_no_reward_is_unrewarded() {
        let a: AccountId = "a.near".parse().unwrap();
        let mut e = Epochs::default();
        assert!(!e.unrewarded(2, Balance::ZERO), "nothing staked");
        e.seed(&a, Balance::from_near(1), 1);
        assert!(e.unrewarded(2, Balance::ZERO));
        assert!(!e.unrewarded(2, Balance::from_yoctonear(1)), "a reward");
        assert!(!e.unrewarded(1, Balance::ZERO), "no boundary");
    }

    /// A write seeds a locked balance it brings in, in the epoch the last
    /// boundary entered and the next, and nothing for an account it gives
    /// none or whose locked balance it only changes.
    #[test]
    fn a_write_seeds_only_a_locked_balance_from_none() {
        use near_primitives::account::{Account, AccountContract};
        use near_primitives::state_record::StateRecord;
        let mut sim = crate::Sim::new();
        sim.create_account("a.near", Balance::from_near(10));
        assert!(sim.chain.log.epochs.stakes.is_empty());
        let record = |locked| StateRecord::Account {
            account_id: "a.near".parse().unwrap(),
            account: Account::new(
                Balance::from_near(10),
                Balance::from_yoctonear(locked),
                AccountContract::None,
                100,
            ),
        };
        sim.apply_state_records([record(7)]);
        sim.set_balance("a.near", Balance::from_near(5));
        sim.apply_state_records([record(9)]);
        let a: AccountId = "a.near".parse().unwrap();
        let seeded = |e| (e, Stakes::from([(a.clone(), Balance::from_yoctonear(7))]));
        assert_eq!(sim.chain.log.epochs.stakes, BTreeMap::from([1, 2].map(seeded)));
    }
}
