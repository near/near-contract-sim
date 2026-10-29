//! Epochs: `env::epoch_height()` follows the height, a stake an account
//! lowers comes back when nearcore's epoch manager would return it, and
//! epoch rewards, off by default, add to the stake. A locked balance an
//! account record brings in is staked, as a genesis validator's is. Epochs
//! are 10 blocks here unless a test says otherwise.

mod common;
use common::*;
use near_contract_sim::nearcore::near_primitives::account::{Account, AccountContract};
use near_contract_sim::nearcore::near_primitives::action::StakeAction;
use near_contract_sim::{
    Action, ActionErrorKind, Balance, Config, Failure, Fault, IntoAccountId, KeyType, Receipts,
    SecretKey, Sim, StateRecord,
};
use std::panic::{AssertUnwindSafe, catch_unwind};

fn sim() -> Sim {
    let mut sim = sim_with_users();
    sim.config.epoch_length = 10;
    sim
}

fn stake_action(sim: &Sim, id: &str, stake: Balance) -> Action {
    StakeAction { stake, public_key: sim.secret_key(id).public_key() }.into()
}

/// `id` stakes `stake` in block `at`, a transaction to itself.
fn stake_at(sim: &mut Sim, id: &str, at: u64, stake: Balance) {
    sim.advance_to(at - 1);
    let action = stake_action(sim, id, stake);
    let tx = sim.tx(id, id).action(action).submit();
    sim.settle();
    let out = sim.outcome(tx);
    out.assert_success();
    assert_eq!(out.receipts[0].block_height, at);
}

fn locked(sim: &Sim, id: impl IntoAccountId) -> Balance {
    sim.account(id).unwrap().locked
}

/// Gives `id` `locked` through its account record, as a genesis or
/// `sandbox_patch_state` would, keeping its liquid balance and storage.
fn import_locked(sim: &mut Sim, id: &str, locked: Balance) {
    let a = sim.account(id).unwrap();
    let account = Account::new(a.amount, locked, AccountContract::None, a.storage_usage);
    sim.apply_state_records([StateRecord::Account { account_id: id.parse().unwrap(), account }]);
}

/// A sim with `epoch_length` blocks per epoch and pool.near, `wasm` (a
/// build of tests/res/staking_pool.wat), holding 100 NEAR alice staked.
/// The pool starts with `balance`: 2 NEAR covers its storage, so a payout
/// the stake doesn't back fails.
fn pool_sim(epoch_length: u64, wasm: Vec<u8>, balance: Balance) -> Sim {
    let mut sim = sim_with_users();
    sim.config.epoch_length = epoch_length;
    sim.create_account("pool.near", balance);
    sim.deploy("pool.near", wasm);
    let key = SecretKey::from_seed(KeyType::ED25519, "pool.near").public_key();
    let args = borsh::to_vec(&key).unwrap();
    sim.call("pool.near", "pool.near", "new").args(args).transact().assert_success();
    let deposit = sim.call("alice.near", "pool.near", "deposit_and_stake").deposit(near(100));
    deposit.transact().assert_success();
    assert_eq!(locked(&sim, "pool.near"), near(100));
    sim
}

/// The chain-side invariant on pool.near: its `locked` is at most what its
/// books count staked plus what `users` unstaked that is still in its
/// cooldown, so whatever the pool lets users withdraw the chain has freed.
fn backed(sim: &Sim, users: &[&str]) -> bool {
    let read = |key: &[u8]| sim.state("pool.near", key).unwrap_or_default();
    let number = |v: Vec<u8>| v.iter().rev().fold(0u128, |n, b| n << 8 | u128::from(*b));
    let cooling: u128 = users
        .iter()
        .filter(|id| {
            number(read(&[&b"e"[..], id.as_bytes()].concat())) > u128::from(sim.epoch_height())
        })
        .map(|id| number(read(&[&b"u"[..], id.as_bytes()].concat())))
        .sum();
    locked(sim, "pool.near").as_yoctonear() <= number(read(b"*total")) + cooling
}

#[test]
fn epoch_height_follows_the_height() {
    assert_eq!(
        (Config::default().epoch_length, Config::default().epoch_reward),
        (43_200, Balance::ZERO)
    );
    let mut sim = sim();
    sim.deploy("pool.near", staking_pool_wasm());
    let epoch = |sim: &Sim| {
        let v = sim.view_raw("pool.near", "epoch_height", b"".to_vec()).result.unwrap();
        u64::from_le_bytes(v.try_into().unwrap())
    };
    sim.set_height(0);
    assert_eq!((epoch(&sim), sim.epoch_height()), (1, 1), "genesis");
    assert_eq!([1, 2, 7].map(|e| sim.epoch_start(e)), [1, 11, 61]);
    let heights = [0, 1, 10, 11, 20, 21, 53];
    assert_eq!(heights.map(|h| sim.epoch_of(h)), [1, 1, 1, 2, 2, 3, 6]);
    // Epoch 1 is blocks 1 to 10 after genesis; a fresh node's would run on
    // to block 20.
    for (h, e) in [(1, 1), (10, 1), (11, 2), (20, 2), (21, 3), (53, 6)] {
        sim.advance_to(h);
        assert_eq!((epoch(&sim), sim.epoch_height()), (e, e), "at block {h}");
    }
    // Converted in epoch 6's last block, the call runs in epoch 7's first.
    sim.advance_to(sim.epoch_start(7) - 2);
    let out = sim.call("alice.near", "pool.near", "epoch_height").transact();
    assert_eq!(out.value().unwrap(), 7u64.to_le_bytes());
    assert_eq!(sim.epoch_of(out.receipts[0].block_height), 7);
    sim.advance_epochs(2);
    assert_eq!((sim.height(), epoch(&sim)), (sim.epoch_start(9), 9));
    sim.advance_epochs(0);
    assert_eq!(sim.height(), sim.epoch_start(9));
    // From an epoch's last block, the next epoch is one block on.
    sim.advance_to(sim.epoch_start(10) - 1);
    sim.advance_epochs(1);
    assert_eq!(sim.height(), sim.epoch_start(10));
    let r = catch_unwind(AssertUnwindSafe(|| sim.epoch_start(0)));
    assert_eq!(panic_message(r), "epoch_start: epoch 0 or an overflow");
}

/// Under 3 blocks an epoch can't hold a call and its stake receipt, so
/// blocks, views and the epoch helpers refuse it; 3 is enough for the pool.
#[test]
fn an_epoch_needs_three_blocks() {
    for n in [0, 1, 2] {
        let mut sim = sim();
        sim.deploy("pool.near", staking_pool_wasm());
        sim.config.epoch_length = n;
        let want = format!(
            "config.epoch_length is {n}: an epoch needs at least 3 blocks (under 3, a stake's \
             return and the epoch a call runs in shift)"
        );
        let view = |sim: &Sim| sim.view_raw("pool.near", "epoch_height", b"".to_vec());
        let r = catch_unwind(AssertUnwindSafe(|| view(&sim)));
        assert_eq!(panic_message(r), want);
        let r = catch_unwind(AssertUnwindSafe(|| sim.clone().step()));
        assert_eq!(panic_message(r), want);
        let r = catch_unwind(AssertUnwindSafe(|| sim.clone().advance_epochs(1)));
        assert_eq!(panic_message(r), want);
        let r = catch_unwind(AssertUnwindSafe(|| sim.epoch_height()));
        assert_eq!(panic_message(r), want);
        let r = catch_unwind(AssertUnwindSafe(|| sim.epoch_start(2)));
        assert_eq!(panic_message(r), want);
        sim.advance_epochs(0);
        let (h, t) = (sim.height(), sim.timestamp_ns());
        let r = catch_unwind(AssertUnwindSafe(|| sim.advance_to(h + 50)));
        assert_eq!(panic_message(r), want);
        assert_eq!((sim.height(), sim.timestamp_ns()), (h, t), "moved before panicking");
    }
}

/// The chain's epochs are counted in the first block's `epoch_length`, so a
/// later change would move every stake's return: blocks, views and the
/// epoch helpers refuse it.
#[test]
fn epoch_length_is_set_before_the_first_block() {
    let mut sim = sim();
    sim.deploy("pool.near", staking_pool_wasm());
    sim.config.epoch_length = 20;
    let want = "config.epoch_length is 20, but the chain's epochs were counted in 10 blocks: \
                set it before the first block";
    let view = |sim: &Sim| sim.view_raw("pool.near", "epoch_height", b"".to_vec());
    assert_eq!(panic_message(catch_unwind(AssertUnwindSafe(|| view(&sim)))), want);
    assert_eq!(panic_message(catch_unwind(AssertUnwindSafe(|| sim.epoch_height()))), want);
    assert_eq!(panic_message(catch_unwind(AssertUnwindSafe(|| sim.epoch_start(2)))), want);
    let r = catch_unwind(AssertUnwindSafe(|| sim.clone().step()));
    assert_eq!(panic_message(r), want);
    let (h, t) = (sim.height(), sim.timestamp_ns());
    let r = catch_unwind(AssertUnwindSafe(|| sim.advance_to(h + 50)));
    assert_eq!(panic_message(r), want);
    assert_eq!((sim.height(), sim.timestamp_ns()), (h, t), "moved before panicking");
    sim.config.epoch_length = 10;
    sim.step();
}

/// A far idle skip with a stake live is one step, not one per epoch, and
/// pays what each epoch would: alice earns from epoch 3, when her stake
/// counted in epoch 1 starts.
#[test]
fn a_far_skip_with_a_stake_is_quick_and_exact() {
    let mut sim = sim();
    sim.config.block_time = std::time::Duration::from_nanos(1);
    sim.config.epoch_reward = Balance::from_yoctonear(1);
    stake_at(&mut sim, "alice.near", 2, near(100));
    for e in [6, 20, 1_000_000_000_000_000] {
        let mut sim = sim.clone();
        sim.advance_to(sim.epoch_start(e));
        let earned = Balance::from_yoctonear(u128::from(e - 3));
        assert_eq!(locked(&sim, "alice.near"), near(100).checked_add(earned).unwrap());
        sim.findings().assert_none();
    }
}

/// An unstake counted in epoch `e` frees the stake at epoch `e + 3`'s first
/// block, not a block earlier, and the stake stays locked until then. A
/// block's proposals count in the next block's epoch, so an unstake in an
/// epoch's last block frees it a boundary later. No reward by default.
#[test]
fn an_unstake_frees_the_stake_three_epochs_after_the_one_it_counts_in() {
    for (e, offset, freed) in [(4, 3, 7), (5, -1, 8)] {
        let mut sim = sim();
        let at = sim.epoch_start(e).checked_add_signed(offset).unwrap();
        let freed = sim.epoch_start(freed);
        stake_at(&mut sim, "alice.near", 2, near(100));
        sim.advance_to(at - 1);
        assert_eq!(locked(&sim, "alice.near"), near(100));
        stake_at(&mut sim, "alice.near", at, Balance::ZERO);
        sim.advance_to(freed - 1);
        assert_eq!(locked(&sim, "alice.near"), near(100), "unstaked in block {at}");
        let before = sim.balance("alice.near");
        sim.step();
        assert_eq!(locked(&sim, "alice.near"), Balance::ZERO, "unstaked in block {at}");
        assert_eq!(sim.balance("alice.near").checked_sub(before), Some(near(100)));
        sim.findings().assert_none();
    }
}

/// The last proposal in an epoch counts: a stake taken back in the epoch
/// it was made in comes back at the next boundary.
#[test]
fn a_stake_unstaked_in_its_own_epoch_comes_back_at_the_next_boundary() {
    let mut sim = sim();
    let at = sim.epoch_start(2) + 1;
    stake_at(&mut sim, "alice.near", at, near(100));
    let at = sim.epoch_start(2) + 4;
    stake_at(&mut sim, "alice.near", at, Balance::ZERO);
    sim.advance_to(sim.epoch_start(3) - 1);
    assert_eq!(locked(&sim, "alice.near"), near(100));
    sim.step();
    assert_eq!(locked(&sim, "alice.near"), Balance::ZERO);
}

/// Two stakes in one transaction: the higher one locks at once, and the
/// last one counts, as the runtime keeps an account's last proposal.
#[test]
fn a_blocks_last_proposal_counts() {
    let mut sim = sim();
    sim.advance_to(3);
    let actions = [near(10), near(5)].map(|s| stake_action(&sim, "alice.near", s));
    let tx =
        sim.tx("alice.near", "alice.near").action(actions[0].clone()).action(actions[1].clone());
    tx.transact().assert_success();
    assert_eq!(locked(&sim, "alice.near"), near(10));
    sim.advance_epochs(5);
    assert_eq!(locked(&sim, "alice.near"), near(5));
    sim.findings().assert_none();
}

/// A stake raised in an epoch's last block counts in the next epoch, and
/// the boundary between keeps it locked (nearcore's `last_proposals`).
#[test]
fn a_stake_raised_in_an_epochs_last_block_stays_locked() {
    let mut sim = sim();
    stake_at(&mut sim, "alice.near", 2, near(100));
    let at = sim.epoch_start(4) - 1;
    stake_at(&mut sim, "alice.near", at, near(150));
    assert_eq!(locked(&sim, "alice.near"), near(150));
    for _ in 0..4 {
        sim.advance_epochs(1);
        assert_eq!(locked(&sim, "alice.near"), near(150), "at block {}", sim.height());
    }
    sim.findings().assert_none();
}

/// Heights skipped over several boundaries by an idle `advance_to` leave
/// every account as producing each block would, rewards included.
#[test]
fn skipped_boundaries_add_up() {
    let mut sim = sim();
    sim.config.epoch_reward = near(1);
    stake_at(&mut sim, "alice.near", 2, near(100));
    stake_at(&mut sim, "bob.near", 6, near(50));
    let at = sim.epoch_start(3) + 2;
    stake_at(&mut sim, "alice.near", at, Balance::ZERO);
    let before = sim.balance("alice.near");
    let (mut stepped, mut split) = (sim.clone(), sim.clone());
    sim.advance_to(sim.epoch_start(9));
    while stepped.height() < stepped.epoch_start(9) {
        stepped.step();
    }
    split.advance_to(split.epoch_start(5) + 3);
    split.advance_to(split.epoch_start(9));
    let view =
        |s: &Sim| ["alice.near", "bob.near"].map(|id| s.account(id).map(|a| (a.amount, a.locked)));
    assert_eq!(view(&stepped), view(&sim));
    assert_eq!(view(&split), view(&sim));
    // Alice earned for epochs 3 and 4, bob for 3 to 8.
    assert_eq!(locked(&sim, "alice.near"), Balance::ZERO);
    assert_eq!(sim.balance("alice.near").checked_sub(before), Some(near(102)));
    assert_eq!(locked(&sim, "bob.near"), near(56));
    for s in [&sim, &stepped, &split] {
        s.findings().assert_none();
    }
}

/// Each epoch an account is staked in pays `epoch_reward` into its stake
/// at the next epoch's first block, a stake proposed again keeps it, and an
/// unstake earns nothing past its last staked epoch.
#[test]
fn rewards_add_to_the_stake_each_epoch() {
    let mut sim = sim();
    sim.config.epoch_reward = near(1);
    // Counted in epoch 1, so staked from epoch 3.
    stake_at(&mut sim, "alice.near", 2, near(100));
    for e in 2..=5 {
        sim.advance_to(sim.epoch_start(e));
        assert_eq!(locked(&sim, "alice.near"), near(100 + u128::from(e.saturating_sub(3))));
    }
    let at = sim.epoch_start(5) + 2;
    stake_at(&mut sim, "alice.near", at, near(102));
    for e in 6..=8 {
        sim.advance_to(sim.epoch_start(e));
        assert_eq!(locked(&sim, "alice.near"), near(100 + u128::from(e - 3)));
    }
    // Staked through epoch 9: what epochs 8 and 9 pay comes straight back.
    let at = sim.epoch_start(8) + 2;
    stake_at(&mut sim, "alice.near", at, Balance::ZERO);
    for (e, stake) in [(9, 105), (10, 105), (11, 0)] {
        sim.advance_to(sim.epoch_start(e));
        assert_eq!(locked(&sim, "alice.near"), near(stake), "epoch {e}");
    }
    sim.findings().assert_none();
}

/// A stake proposed by an account that has left the next epoch's set
/// starts from the proposal: what it earned in its last epoch comes back.
#[test]
fn a_restake_after_leaving_starts_from_the_proposal() {
    let mut sim = sim();
    sim.config.epoch_reward = near(1);
    stake_at(&mut sim, "alice.near", 2, near(100));
    let at = sim.epoch_start(2) + 2;
    stake_at(&mut sim, "alice.near", at, Balance::ZERO);
    let at = sim.epoch_start(3) + 2;
    stake_at(&mut sim, "alice.near", at, near(50));
    sim.advance_to(sim.epoch_start(4));
    assert_eq!(locked(&sim, "alice.near"), near(100));
    sim.advance_to(sim.epoch_start(5));
    assert_eq!(locked(&sim, "alice.near"), near(50));
    sim.advance_to(sim.epoch_start(6));
    assert_eq!(locked(&sim, "alice.near"), near(51));
    sim.findings().assert_none();
}

/// A far skip right after an unstake has run its course: the boundaries
/// between have nothing left to pay, and the stake comes back.
#[test]
fn a_far_skip_after_an_unstake_frees_the_stake() {
    let mut sim = sim();
    sim.config.block_time = std::time::Duration::from_nanos(1);
    sim.config.epoch_reward = near(1);
    stake_at(&mut sim, "alice.near", 2, near(100));
    let at = sim.epoch_start(5) + 3;
    stake_at(&mut sim, "alice.near", at, Balance::ZERO);
    sim.advance_to(sim.epoch_start(1_000_000_000_000_000));
    assert_eq!(locked(&sim, "alice.near"), Balance::ZERO);
    sim.findings().assert_none();
}

/// Rewound, the epoch height follows the height, but no boundary repeats:
/// an unstake counts in the furthest epoch reached.
#[test]
fn a_rewind_repeats_no_boundary() {
    let mut sim = sim();
    stake_at(&mut sim, "alice.near", 2, near(100));
    sim.advance_to(sim.epoch_start(5));
    sim.set_height(2);
    stake_at(&mut sim, "alice.near", 4, Balance::ZERO);
    sim.advance_to(sim.epoch_start(8) - 1);
    assert_eq!(locked(&sim, "alice.near"), near(100));
    sim.step();
    assert_eq!(locked(&sim, "alice.near"), Balance::ZERO);
}

/// A snapshot keeps the epoch config and the stake still to come back.
#[test]
fn a_snapshot_keeps_the_stake_to_come_back() {
    let (key, setups) = (std::time::SystemTime::now(), std::cell::Cell::new(0));
    let cached = || {
        Sim::cached(key, || {
            setups.set(setups.get() + 1);
            let mut sim = sim();
            sim.config.epoch_reward = near(1);
            sim.config.minimum_stake = near(50);
            stake_at(&mut sim, "alice.near", 2, near(100));
            let at = sim.epoch_start(3) + 2;
            stake_at(&mut sim, "alice.near", at, Balance::ZERO);
            sim
        })
    };
    cached();
    let mut sim = cached();
    assert_eq!(setups.get(), 1, "saved, then loaded");
    let c = &sim.config;
    assert_eq!((c.epoch_length, c.epoch_reward, c.minimum_stake), (10, near(1), near(50)));
    sim.advance_to(sim.epoch_start(6) - 1);
    assert_eq!(locked(&sim, "alice.near"), near(100));
    sim.step();
    assert_eq!(locked(&sim, "alice.near"), Balance::ZERO);
    sim.findings().assert_none();
}

/// A locked balance a record brings in is staked in the current epoch and
/// the next, as a genesis validator's is: an unstake counted in epoch `e`
/// frees it at `e + 3`'s first block, not a block earlier, as a stake the
/// account made itself.
#[test]
fn an_imported_stake_waits_out_the_delay() {
    for (e, offset, freed) in [(1, 5, 4), (4, 3, 7), (5, -1, 8)] {
        let mut sim = sim();
        let at = sim.epoch_start(e).checked_add_signed(offset).unwrap();
        let freed = sim.epoch_start(freed);
        import_locked(&mut sim, "alice.near", near(100));
        sim.findings().assert_none();
        sim.advance_to(at - 1);
        assert_eq!(locked(&sim, "alice.near"), near(100));
        stake_at(&mut sim, "alice.near", at, Balance::ZERO);
        sim.advance_to(freed - 1);
        assert_eq!(locked(&sim, "alice.near"), near(100), "unstaked in block {at}");
        sim.findings().assert_none();
        let before = sim.balance("alice.near");
        sim.step();
        assert_eq!(locked(&sim, "alice.near"), Balance::ZERO, "unstaked in block {at}");
        assert_eq!(sim.balance("alice.near").checked_sub(before), Some(near(100)));
        sim.findings().assert_none();
    }
}

/// Staked from genesis, an imported stake earns from epoch 1, where one
/// made in block 2 earns from epoch 3, and a far skip pays it every epoch.
#[test]
fn an_imported_stake_earns_rewards() {
    let mut sim = sim();
    sim.config.epoch_reward = near(1);
    import_locked(&mut sim, "alice.near", near(100));
    stake_at(&mut sim, "bob.near", 2, near(100));
    for e in 2..=5 {
        sim.advance_to(sim.epoch_start(e));
        assert_eq!(locked(&sim, "alice.near"), near(100 + u128::from(e - 1)), "epoch {e}");
        assert_eq!(locked(&sim, "bob.near"), near(100 + u128::from(e.saturating_sub(3))));
        sim.findings().assert_none();
    }
    sim.advance_to(sim.epoch_start(40));
    assert_eq!(locked(&sim, "alice.near"), near(139));
    assert_eq!(locked(&sim, "bob.near"), near(137));
    sim.findings().assert_none();
}

/// Imported after blocks, a stake counts from the epoch the last block
/// is in: it earns for that epoch and none before, and its unstake waits
/// out the delay.
#[test]
fn a_stake_imported_after_blocks_counts_from_the_current_epoch() {
    let mut sim = sim();
    sim.config.epoch_reward = near(1);
    sim.advance_to(sim.epoch_start(4) + 2);
    import_locked(&mut sim, "alice.near", near(50));
    sim.findings().assert_none();
    sim.advance_to(sim.epoch_start(5));
    assert_eq!(locked(&sim, "alice.near"), near(51));
    let at = sim.epoch_start(6) + 3;
    stake_at(&mut sim, "alice.near", at, Balance::ZERO);
    // Staked through epoch 7: what epochs 6 and 7 pay comes straight back.
    sim.advance_to(sim.epoch_start(9) - 1);
    assert_eq!(locked(&sim, "alice.near"), near(52));
    let before = sim.balance("alice.near");
    sim.step();
    assert_eq!(locked(&sim, "alice.near"), Balance::ZERO);
    assert_eq!(sim.balance("alice.near").checked_sub(before), Some(near(52)));
    sim.findings().assert_none();
}

/// Imported after a rewind, a stake counts from the furthest epoch reached,
/// as an unstake does.
#[test]
fn a_stake_imported_after_a_rewind_counts_from_the_furthest_epoch() {
    let mut sim = sim();
    sim.advance_to(sim.epoch_start(5));
    sim.set_height(2);
    import_locked(&mut sim, "alice.near", near(100));
    stake_at(&mut sim, "alice.near", 4, Balance::ZERO);
    sim.advance_to(sim.epoch_start(8) - 1);
    assert_eq!(locked(&sim, "alice.near"), near(100));
    sim.step();
    assert_eq!(locked(&sim, "alice.near"), Balance::ZERO);
    sim.findings().assert_none();
}

/// A snapshot, and a clone of it, keep an imported stake.
#[test]
fn a_snapshot_and_a_clone_keep_an_imported_stake() {
    let (key, setups) = (std::time::SystemTime::now(), std::cell::Cell::new(0));
    let cached = || {
        Sim::cached(key, || {
            setups.set(setups.get() + 1);
            let mut sim = sim();
            sim.config.epoch_reward = near(1);
            import_locked(&mut sim, "alice.near", near(100));
            sim
        })
    };
    cached();
    let loaded = cached();
    assert_eq!(setups.get(), 1, "saved, then loaded");
    for mut sim in [loaded.clone(), loaded] {
        let at = sim.epoch_start(2) + 2;
        stake_at(&mut sim, "alice.near", at, Balance::ZERO);
        // Staked through epoch 3: what epochs 2 and 3 pay comes straight back.
        sim.advance_to(sim.epoch_start(5) - 1);
        assert_eq!(locked(&sim, "alice.near"), near(101));
        sim.step();
        assert_eq!(locked(&sim, "alice.near"), Balance::ZERO);
        sim.findings().assert_none();
    }
}

/// A record that changes a staked account's locked balance leaves its
/// stake as it was, as a node's epoch manager learns of no write, even one
/// that takes it through 0: raised, the next boundary hands the excess
/// back; lowered below the stake, even short of 0, nearcore's staking
/// check fails the boundary.
#[test]
fn a_record_over_an_imported_stake_leaves_the_stake() {
    for through_zero in [false, true] {
        let mut sim = sim();
        import_locked(&mut sim, "alice.near", near(100));
        if through_zero {
            import_locked(&mut sim, "alice.near", Balance::ZERO);
        }
        let mut lowered = sim.clone();
        import_locked(&mut sim, "alice.near", near(150));
        sim.advance_to(sim.epoch_start(2) - 1);
        assert_eq!(locked(&sim, "alice.near"), near(150), "through 0: {through_zero}");
        let before = sim.balance("alice.near");
        sim.step();
        assert_eq!(locked(&sim, "alice.near"), near(100), "through 0: {through_zero}");
        assert_eq!(sim.balance("alice.near").checked_sub(before), Some(near(50)));
        sim.findings().assert_none();

        import_locked(&mut lowered, "alice.near", near(50));
        let r = catch_unwind(AssertUnwindSafe(|| lowered.advance_epochs(1)));
        let m = panic_message(r);
        let want = "Runtime::apply failed at the block that applies epoch 2's boundary: ";
        assert!(m.starts_with(want), "through 0: {through_zero}: {m}");
        assert!(m.ends_with("did a direct write (`apply_state_records`) lower it?"), "{m}");
    }
}

/// A record that raises the locked balance of an account whose unstake is
/// in its last epoch stakes nothing: the boundary that frees the stake
/// hands back all of it.
#[test]
fn a_record_over_a_leaving_stake_comes_back_with_it() {
    let mut sim = sim();
    import_locked(&mut sim, "alice.near", near(100));
    stake_at(&mut sim, "alice.near", 6, Balance::ZERO);
    sim.advance_to(sim.epoch_start(3) + 4);
    import_locked(&mut sim, "alice.near", near(150));
    sim.advance_to(sim.epoch_start(4) - 1);
    assert_eq!(locked(&sim, "alice.near"), near(150));
    let before = sim.balance("alice.near");
    sim.step();
    assert_eq!(locked(&sim, "alice.near"), Balance::ZERO);
    assert_eq!(sim.balance("alice.near").checked_sub(before), Some(near(150)));
    sim.findings().assert_none();
}

/// A record that takes a proposed stake's locked balance through 0 leaves
/// the proposal as the only stake: it earns from the epoch after next, as
/// a stake made in block 2 does, not from the current one.
#[test]
fn a_record_through_zero_leaves_a_proposed_stake() {
    let mut sim = sim();
    sim.config.epoch_reward = near(1);
    stake_at(&mut sim, "alice.near", 2, near(100));
    import_locked(&mut sim, "alice.near", Balance::ZERO);
    import_locked(&mut sim, "alice.near", near(100));
    for e in 2..=5 {
        sim.advance_to(sim.epoch_start(e));
        assert_eq!(locked(&sim, "alice.near"), near(100 + u128::from(e.saturating_sub(3))));
        sim.findings().assert_none();
    }
}

/// After a forward `set_height` with nothing staked or proposed, an
/// imported stake counts from the epoch the height is in, so the next
/// block pays it for that epoch only. With a stake live, imported or
/// proposed, it counts from the epoch the last boundary entered (the last
/// produced block's, or the furthest reached), as that stake does, so the
/// next block pays it for every epoch skipped.
#[test]
fn a_stake_imported_after_a_forward_set_height() {
    {
        let mut sim = sim();
        sim.config.epoch_reward = near(1);
        sim.set_height(90);
        assert_eq!(sim.epoch_height(), 9);
        import_locked(&mut sim, "alice.near", near(100));
        sim.step();
        assert_eq!(locked(&sim, "alice.near"), near(101));
        sim.findings().assert_none();
    }
    {
        let mut sim = sim();
        sim.config.epoch_reward = near(1);
        import_locked(&mut sim, "alice.near", near(100));
        sim.set_height(90);
        import_locked(&mut sim, "bob.near", near(100));
        sim.step();
        assert_eq!(locked(&sim, "alice.near"), near(109));
        assert_eq!(locked(&sim, "bob.near"), near(109));
        sim.findings().assert_none();
    }
    {
        let mut sim = sim();
        sim.config.epoch_reward = near(1);
        // Counted in epoch 1, so staked, and earning, from epoch 3.
        stake_at(&mut sim, "alice.near", 2, near(100));
        sim.set_height(90);
        import_locked(&mut sim, "bob.near", near(100));
        sim.step();
        assert_eq!(locked(&sim, "alice.near"), near(107));
        assert_eq!(locked(&sim, "bob.near"), near(109));
        sim.findings().assert_none();
    }
}

/// validator.near, the validator the runtime sees with 50M NEAR, has no
/// account, so the sim stakes nothing for it: with rewards on it is paid
/// nothing, created it stays unstaked, and conservation holds. A record
/// that gives it a locked balance stakes it as any other account, and
/// `env::validator_stake` stays 50M.
#[test]
fn validator_near_is_staked_only_by_a_record() {
    let mut sim = sim();
    sim.config.epoch_reward = near(1);
    sim.deploy("env.near", env_wasm());
    let stake = |sim: &Sim| {
        let v = sim.view_raw("env.near", "validator_stake", b"validator.near".to_vec());
        u128::from_le_bytes(v.result.unwrap().try_into().unwrap())
    };
    sim.advance_epochs(3);
    assert!(sim.account("validator.near").is_none());
    sim.findings().assert_none();
    let mut created = sim.clone();
    created.create_account("validator.near", near(10));
    created.advance_epochs(3);
    assert_eq!(locked(&created, "validator.near"), Balance::ZERO);
    created.findings().assert_none();

    let account = Account::new(near(10), near(50_000_000), AccountContract::None, 100);
    let account_id = "validator.near".parse().unwrap();
    sim.apply_state_records([StateRecord::Account { account_id, account }]);
    sim.advance_epochs(2);
    assert_eq!(locked(&sim, "validator.near"), near(50_000_002));
    assert_eq!(stake(&sim), near(50_000_000).as_yoctonear());
    sim.findings().assert_none();
}

/// Deposit, stake, unstake in an epoch's last block, wait out the
/// cooldown, withdraw, at the shortest epoch and a longer one. Sent there,
/// the pool's stake receipt runs in the next epoch, where its proposal
/// counts, so nearcore frees the stake 4 epochs after the unstake's.
#[test]
fn a_staking_pool_pays_back_after_its_cooldown() {
    for epoch_length in [3, 10] {
        let mut sim = pool_sim(epoch_length, staking_pool_wasm(), near(2));
        let e = sim.epoch_height() + 1;
        sim.advance_to(sim.epoch_start(e + 1) - 3);
        let out = sim.call("alice.near", "pool.near", "unstake").transact();
        assert_eq!(out.expect_receipt("unstake").block_height, sim.epoch_start(e + 1) - 1);
        let unlock = e + 4;
        sim.advance_to(sim.epoch_start(unlock - 1));
        let early = sim.call("alice.near", "pool.near", "withdraw").transact().assert_failure();
        assert_eq!(early, Failure::Panic("not unlocked".into()));
        sim.advance_to(sim.epoch_start(unlock));
        assert_eq!(sim.account("pool.near").unwrap().locked, Balance::ZERO);
        let before = sim.balance("alice.near");
        sim.call("alice.near", "pool.near", "withdraw").transact().assert_success();
        assert!(sim.balance("alice.near") > before.checked_add(near(99)).unwrap());
        sim.findings().assert_none();
    }
}

/// The same pool counting 3 epochs pays that withdraw an epoch before
/// nearcore frees the stake. Funded with only its storage and setup gas,
/// the payout fails; with 1,000 NEAR it goes through, out of the
/// pool's liquid balance. Either way the chain-side invariant catches the
/// pool letting it.
#[test]
fn a_three_epoch_cooldown_pays_out_before_the_stake_is_free() {
    let path = manifest_dir().join("tests/res/staking_pool.wat");
    let wat = std::fs::read_to_string(path).unwrap();
    let three = wat.replace("(i64.const 4))) ;; cooldown", "(i64.const 3))) ;; cooldown");
    assert_ne!(three, wat);
    for balance in [near(2), near(1_000)] {
        let mut sim = pool_sim(10, wat::parse_str(&three).unwrap(), balance);
        let e = sim.epoch_height() + 1;
        sim.advance_to(sim.epoch_start(e + 1) - 3);
        sim.call("alice.near", "pool.near", "unstake").transact().assert_success();
        assert!(backed(&sim, &["alice.near"]));
        sim.advance_to(sim.epoch_start(e + 3));
        let out = sim.call("alice.near", "pool.near", "withdraw").transact();
        assert_eq!(out.is_success(), balance == near(1_000), "{out}");
        assert_eq!(locked(&sim, "pool.near"), near(100));
        assert!(!backed(&sim, &["alice.near"]));
    }
}

/// Overlapping cooldowns on the correct pool: alice's withdraw is paid
/// while bob's unstake still holds stake locked, and the invariant counts
/// that stake as bob's, not as alice's payout.
#[test]
fn overlapping_cooldowns_stay_backed() {
    let mut sim = pool_sim(10, staking_pool_wasm(), near(2));
    let users = ["alice.near", "bob.near"];
    let deposit = sim.call("bob.near", "pool.near", "deposit_and_stake").deposit(near(50));
    deposit.transact().assert_success();
    let e = sim.epoch_height() + 1;
    for (who, at) in [("alice.near", e), ("bob.near", e + 2)] {
        sim.advance_to(sim.epoch_start(at) + 3);
        sim.call(who, "pool.near", "unstake").transact().assert_success();
        assert!(backed(&sim, &users));
    }
    for (who, at) in [("alice.near", e + 4), ("bob.near", e + 6)] {
        sim.advance_to(sim.epoch_start(at));
        sim.call(who, "pool.near", "withdraw").transact().assert_success();
        assert!(backed(&sim, &users), "after {who}'s withdraw");
    }
    assert_eq!(locked(&sim, "pool.near"), Balance::ZERO);
    sim.findings().assert_none();
}

/// A fault that fails the receipt holding the pool's stake rolls the stake
/// back with the call after it: nothing is locked or proposed.
#[test]
fn a_faulted_stake_batch_proposes_nothing() {
    let mut sim = pool_sim(10, staking_pool_wasm(), near(2));
    let tx = sim.call("bob.near", "pool.near", "deposit_and_stake").deposit(near(50));
    let out = tx.inject(Receipts::method("epoch_height"), Fault::OutOfGas).transact();
    let batch = out.expect_receipt("epoch_height");
    assert_eq!((batch.failure(), &batch.fault), (Some(Failure::OutOfGas), &Some(Fault::OutOfGas)));
    assert_eq!(locked(&sim, "pool.near"), near(100));
    sim.advance_epochs(4);
    assert_eq!(locked(&sim, "pool.near"), near(100));
}

/// `config.minimum_stake` is what nearcore's `action_stake` checks a
/// positive stake against: under it, the Stake action fails with
/// nearcore's own `InsufficientStake` and locks nothing more, a lowered
/// stake included; at or above it, and a full unstake to 0, it succeeds.
/// 0 by default, so any positive stake passes; the block that runs the
/// action reads it.
#[test]
fn a_stake_under_the_minimum_fails_as_nearcore_fails_it() {
    assert_eq!(Config::default().minimum_stake, Balance::ZERO);
    let mut sim = sim();
    let stake = |sim: &mut Sim, id: &str, amount: Balance| {
        let action = stake_action(sim, id, amount);
        sim.tx(id, id).action(action).transact()
    };
    stake(&mut sim, "alice.near", Balance::from_yoctonear(1)).assert_success();

    let min = near(10);
    sim.config.minimum_stake = min;
    let insufficient = |stake: Balance| {
        Failure::Action(ActionErrorKind::InsufficientStake {
            account_id: "bob.near".parse().unwrap(),
            stake,
            minimum_stake: min,
        })
    };
    let out = stake(&mut sim, "bob.near", near(5));
    assert_eq!(out.failure(), Some(insufficient(near(5))), "{out}");
    assert_eq!(locked(&sim, "bob.near"), Balance::ZERO);
    let one_under = min.checked_sub(Balance::from_yoctonear(1)).unwrap();
    let out = stake(&mut sim, "bob.near", one_under);
    assert_eq!(out.failure(), Some(insufficient(one_under)), "{out}");
    stake(&mut sim, "bob.near", min).assert_success();
    stake(&mut sim, "bob.near", near(15)).assert_success();
    assert_eq!(locked(&sim, "bob.near"), near(15));
    // Lowering a stake below the minimum fails too; leaving (0) doesn't.
    let out = stake(&mut sim, "bob.near", near(5));
    assert_eq!(out.failure(), Some(insufficient(near(5))), "{out}");
    assert_eq!(locked(&sim, "bob.near"), near(15));
    stake(&mut sim, "bob.near", Balance::ZERO).assert_success();
    // The block that runs it reads the minimum, not the one it was
    // submitted under.
    let action = stake_action(&sim, "alice.near", near(5));
    let tx = sim.tx("alice.near", "alice.near").action(action).submit();
    sim.config.minimum_stake = Balance::ZERO;
    sim.settle();
    sim.outcome(tx).assert_success();
}
