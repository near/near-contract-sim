//! External contract calls (NEP-667, protocol 89): a call to a contract's
//! reserved `__external_call` that nobody signs, accepted by returning and
//! paid for by the contract, refused by failing and then free.
//!
//! Verification boundary: nearcore's runtime runs each call (the host
//! function table, the limits, the charge, the receipts' authorization),
//! and these tests pin what it does at the pinned tag through the
//! simulator. The simulator owns what a node does around it: the checks
//! before the pool, the producer's dry run and packing, the repeat window
//! and the heights a call sees. `forced()` runs the same calls through
//! apply alone, so each honest verdict is checked against apply's.
//! `tests/external_calls_vs_sandbox.rs` compares both with a real
//! 2.15.0-rc.1 sandbox.

mod common;
use common::*;
use near_contract_sim::nearcore::near_primitives::errors::{
    ActionErrorKind, InvalidTxError, ReceiptValidationError,
};
use near_contract_sim::nearcore::near_primitives::universal_state_init::{
    RawStateInit, UniversalStateInit, UniversalStateInitV1,
};
use near_contract_sim::nearcore::near_primitives::utils::{
    create_ecc_outcome_id, derive_universal_account_id,
};
use near_contract_sim::{
    AccountId, Balance, ExternalCallOutcome, Failure, Fault, Gas, GlobalContractDeployMode,
    GlobalContractIdentifier, Receipts, Reply, Sim, Stub, TxOutcome,
};
use std::collections::{BTreeMap, BTreeSet};

const PROBE: &str = "probe.near";
const NOT_ALLOWED: &str = "is not allowed in external contract calls";
const RECORDED: &str =
    "Size of the recorded trie storage proof has exceeded the allowed limit (51.2 KB)";

fn sim() -> Sim {
    let mut sim = Sim::new();
    sim.create_account("alice.near", near(100));
    sim.create_account("bob.near", near(100));
    sim.create_account(PROBE, near(100));
    sim.deploy(PROBE, wat("ecc_probe.wat"));
    sim.deploy("bob.near", wat("ecc_probe.wat"));
    sim
}

fn op(code: u8, rest: &[u8]) -> Vec<u8> {
    let mut p = vec![code];
    p.extend(rest);
    p
}

/// Op 13: a call of `method` on `target` with `gas`, `weight` and `deposit`.
fn promise(gas: Gas, weight: u64, deposit: Balance, method: &str, target: &str) -> Vec<u8> {
    let mut p = op(13, &gas.as_gas().to_le_bytes());
    p.extend(weight.to_le_bytes());
    p.extend(deposit.as_yoctonear().to_le_bytes());
    p.push(method.len() as u8);
    p.extend(method.as_bytes());
    p.extend(target.as_bytes());
    p
}

fn u32_op(code: u8, n: u32) -> Vec<u8> {
    op(code, &n.to_le_bytes())
}

/// The same payload as an ordinary call of the probe's `call`, from alice.
fn ordinary(sim: &mut Sim, payload: Vec<u8>) -> TxOutcome {
    sim.call("alice.near", PROBE, "call").args(payload).gas(Gas::from_teragas(100)).transact()
}

fn le_u128(out: &TxOutcome) -> u128 {
    u128::from_le_bytes(out.bytes().try_into().expect("16 bytes"))
}

fn honest(sim: &mut Sim, payload: Vec<u8>) -> ExternalCallOutcome {
    sim.external_call(PROBE, payload).transact()
}

fn forced(sim: &mut Sim, payload: Vec<u8>) -> ExternalCallOutcome {
    sim.external_call(PROBE, payload).forced().transact()
}

/// The verdict a call came to, honest or forced alike: accepted with the
/// gas it burnt, or why it failed.
#[derive(Debug, PartialEq)]
enum Verdict {
    Accepted(Gas),
    Refused(Failure),
}

fn verdict(out: ExternalCallOutcome) -> Verdict {
    match out {
        ExternalCallOutcome::Refused(r) => Verdict::Refused(r.failure()),
        ExternalCallOutcome::Included(o) => {
            let t = o.transaction.expect("an inclusion has the call's outcome");
            match t.failure() {
                Some(f) => {
                    assert_eq!((t.gas_burnt, t.tokens_burnt), (Gas::ZERO, Balance::ZERO));
                    Verdict::Refused(f)
                }
                None => Verdict::Accepted(t.gas_burnt),
            }
        }
        other => panic!("expected an inclusion or a refusal, got {other}"),
    }
}

/// Sends `payload` honestly and forced, each on a fresh chain, checks the
/// producer's dry run came to apply's verdict, and returns it.
fn decided(mk: impl Fn() -> Sim, payload: Vec<u8>) -> Verdict {
    let h = verdict(honest(&mut mk(), payload.clone()));
    let f = verdict(forced(&mut mk(), payload));
    assert_eq!(h, f, "the producer's dry run and apply disagree");
    h
}

fn host(m: &str) -> Verdict {
    Verdict::Refused(Failure::Host(m.into()))
}

// ---- accept and refuse ----

#[test]
fn an_accepted_call_runs_the_reserved_method_and_its_contract_pays_what_it_burnt() {
    let mut sim = sim();
    let before = sim.balance(PROBE);
    let out = sim.external_call(PROBE, op(0, b"hi")).transact().included();
    out.assert_success();
    assert_eq!(out.bytes(), b"hi");
    assert_eq!(out.logs(), ["accepted"]);
    let t = out.transaction.as_ref().unwrap();
    assert!(t.is_external_call());
    assert_eq!((t.receiver_id.as_str(), t.predecessor_id.as_str()), (PROBE, PROBE));
    assert_eq!(t.method_names, ["__external_call"]);
    assert_eq!(t.args, [op(0, b"hi")]);
    assert!(t.gas_burnt > Gas::ZERO);
    let price = sim.config.gas_price.as_yoctonear();
    assert_eq!(t.tokens_burnt.as_yoctonear(), t.gas_burnt.as_gas() as u128 * price);
    assert_eq!(
        before.as_yoctonear() - sim.balance(PROBE).as_yoctonear(),
        t.tokens_burnt.as_yoctonear()
    );
    sim.findings().assert_none();
}

#[test]
fn an_accepted_calls_writes_stay() {
    let mut sim = sim();
    honest(&mut sim, op(2, b"v")).included().assert_success();
    assert_eq!(sim.state(PROBE, b"k").as_deref(), Some(&b"v"[..]));
}

#[test]
fn a_refused_call_is_never_included_rolls_back_and_charges_nobody() {
    let mut sim = sim();
    let (before, txs) = (sim.account(PROBE).unwrap(), sim.txs().len());
    let out = sim.external_call(PROBE, vec![1]).transact();
    assert_eq!(out.assert_refused(), Failure::Panic("refused".into()));
    assert_eq!(sim.account(PROBE).unwrap(), before);
    assert_eq!(sim.state(PROBE, b"w"), None);
    assert_eq!(sim.txs().len(), txs);
}

#[test]
fn a_refused_call_forced_in_fails_burns_nothing_and_rolls_back() {
    let mut sim = sim();
    let before = sim.balance(PROBE);
    let out = forced(&mut sim, vec![1]).included();
    assert_eq!(out.assert_failure(), Failure::Panic("refused".into()));
    let t = out.transaction.as_ref().unwrap();
    assert_eq!((t.gas_burnt, t.tokens_burnt), (Gas::ZERO, Balance::ZERO));
    assert!(t.receipt_ids.is_empty());
    assert_eq!(sim.balance(PROBE), before);
    assert_eq!(sim.state(PROBE, b"w"), None);
    sim.findings().assert_none();
}

#[test]
fn a_refusal_keeps_what_the_dry_run_logged() {
    let out = honest(&mut sim(), vec![19]);
    let r = out.refusal().expect("refused");
    assert_eq!(r.failure(), Failure::Panic("refused".into()));
    assert_eq!(r.logs, ["accepted"]);
    assert!(honest(&mut sim(), vec![1]).refusal().unwrap().logs.is_empty());
}

#[test]
fn an_ordinary_call_of_the_reserved_method_fails() {
    let mut sim = sim();
    let out = sim.call("alice.near", PROBE, "__external_call").transact();
    let f = out.assert_failure();
    assert!(format!("{f:?}").contains("ReservedMethodName"), "{f:?}");
    assert_eq!(sim.state(PROBE, b"w"), None);
}

// ---- the host function table ----

#[test]
fn host_functions_without_a_value_in_an_external_call_refuse_it() {
    for (code, name) in [
        (3, "signer_account_pk"),
        (4, "predecessor_account_id"),
        (5, "prepaid_gas"),
        (6, "random_seed"),
        (7, "promise_yield_create"),
        (8, "promise_yield_create_with_id"),
    ] {
        let want = host(&format!("{name} {NOT_ALLOWED}"));
        assert_eq!(decided(sim, vec![code]), want, "{name}");
        ordinary(&mut sim(), vec![code]).assert_success();
    }
}

#[test]
fn the_contract_stands_in_for_the_signer_and_the_refund_receiver() {
    for code in [9, 10] {
        let out = honest(&mut sim(), vec![code]).included();
        assert_eq!(out.bytes(), PROBE.as_bytes(), "op {code}");
        assert_eq!(ordinary(&mut sim(), vec![code]).bytes(), b"alice.near", "op {code}");
    }
}

#[test]
fn nothing_is_attached_and_the_balance_is_the_one_before_the_charge() {
    let mut s = sim();
    assert_eq!(le_u128(&honest(&mut s, vec![11]).included()), 0);
    let before = s.balance(PROBE).as_yoctonear();
    let out = honest(&mut s, vec![12]).included();
    assert_eq!(le_u128(&out), before);
    assert!(s.balance(PROBE).as_yoctonear() < before);
}

#[test]
fn the_run_sees_the_chunks_parent_block() {
    for pin in [None, Some(500)] {
        let mut s = sim();
        if let Some(h) = pin {
            s.set_height(h);
        }
        let (parent, parent_ns) = match pin {
            Some(h) => (h - 1, None),
            None => (s.height(), Some(s.timestamp_ns())),
        };
        let out = honest(&mut s, vec![17]).included();
        let v = out.bytes();
        let (index, ns) = (
            u64::from_le_bytes(v[..8].try_into().unwrap()),
            u64::from_le_bytes(v[8..].try_into().unwrap()),
        );
        assert_eq!(out.transaction.as_ref().unwrap().block_height, parent + 1);
        assert_eq!(index, parent, "pin {pin:?}");
        if let Some(p) = parent_ns {
            assert_eq!(ns, p);
        }
        assert_eq!(
            ns + s.config.block_time.as_nanos() as u64,
            out.transaction.unwrap().block_timestamp_ns
        );
    }
}

// ---- promises ----

#[test]
fn a_promise_gets_exactly_the_gas_it_attached_and_its_weight_is_ignored() {
    let p = promise(Gas::from_teragas(5), 1, Balance::ZERO, "ping", "bob.near");
    let out = honest(&mut sim(), p.clone()).included();
    out.assert_success();
    assert_eq!(out.bytes(), b"pong");
    assert_eq!(out.expect_receipt("ping").attached_gas, Gas::from_teragas(5));
    let o = ordinary(&mut sim(), p);
    assert!(o.expect_receipt("ping").attached_gas > Gas::from_teragas(5));
}

#[test]
fn a_promise_with_no_gas_refuses_the_call_whatever_its_weight() {
    let p = promise(Gas::ZERO, 1, Balance::ZERO, "ping", "bob.near");
    let zero = ReceiptValidationError::ActionsValidation(
        near_contract_sim::nearcore::near_primitives::errors::ActionsValidationError::FunctionCallZeroAttachedGas,
    );
    let want = Verdict::Refused(Failure::Action(ActionErrorKind::NewReceiptValidationError(zero)));
    assert_eq!(decided(sim, p.clone()), want);
    ordinary(&mut sim(), p).assert_success();
}

#[test]
fn the_contract_pays_the_gas_and_the_deposit_it_attaches_and_gets_the_refunds() {
    let mut s = sim();
    let (probe, bob) = (s.balance(PROBE).as_yoctonear(), s.balance("bob.near").as_yoctonear());
    let p = promise(Gas::from_teragas(5), 0, near(2), "ping", "bob.near");
    let sent = s.external_call(PROBE, p).submit();
    s.step();
    let at_inclusion = s.balance(PROBE).as_yoctonear();
    s.settle();
    let out = s.external_call_outcome(sent).included();
    out.assert_success();
    let t = out.transaction.as_ref().unwrap();
    let child = out.expect_receipt("ping");
    // The gas it attaches is bought at the receipt gas price, nearcore's
    // `receipt_gas_price`: the block's, but at least the minimum purchase price.
    let rc = s.config.runtime_config();
    let price = s.config.gas_price.max(rc.min_gas_purchase_price).as_yoctonear();
    let prepaid = (child.attached_gas.as_gas() + child.prepaid_exec_fees.as_gas()) as u128;
    // At inclusion: what it burnt, the gas it attached and its fees, the deposit.
    let charged = t.tokens_burnt.as_yoctonear() + prepaid * price + near(2).as_yoctonear();
    assert_eq!(probe - at_inclusion, charged);
    // Once settled: the gas it all burnt and the deposit; the rest came back.
    let spent = out.tokens_burnt().as_yoctonear() + near(2).as_yoctonear();
    assert_eq!(probe - s.balance(PROBE).as_yoctonear(), spent);
    let bob_burnt = out.expect_receipt("ping").tokens_burnt.as_yoctonear();
    assert!(s.balance("bob.near").as_yoctonear() + bob_burnt >= bob + near(2).as_yoctonear());
    s.findings().assert_none();
}

#[test]
fn every_receipt_down_the_chain_of_an_external_call_has_no_signer_key() {
    // bob.pk reads signer_account_pk; bob.fwd calls bob.pk.
    for method in ["pk", "fwd"] {
        let p = promise(Gas::from_teragas(30), 0, Balance::ZERO, method, "bob.near");
        let mut s = sim();
        let out = honest(&mut s, p.clone()).included();
        let pk = out.expect_receipt("pk");
        assert_eq!(pk.failure(), Some(Failure::Host(format!("signer_account_pk {NOT_ALLOWED}"))));
        // The call itself was accepted and paid for.
        assert!(out.transaction.as_ref().unwrap().is_success());
        s.findings().assert_none();
        // From a signed transaction the same chain reads alice's key.
        ordinary(&mut sim(), p).assert_success();
    }
}

// ---- limits ----

/// Finds the largest `n` in `lo..hi` whose payload is accepted, given that
/// `lo` is and `hi` isn't, checking each probe honest and forced.
fn boundary(
    mk: impl Fn() -> Sim + Copy,
    payload: impl Fn(u64) -> Vec<u8>,
    lo: u64,
    hi: u64,
) -> (u64, Verdict) {
    let (mut lo, mut hi) = (lo, hi);
    assert!(matches!(decided(mk, payload(lo)), Verdict::Accepted(_)));
    let mut past = decided(mk, payload(hi));
    assert!(matches!(past, Verdict::Refused(_)));
    while hi - lo > 1 {
        let m = lo + (hi - lo) / 2;
        match decided(mk, payload(m)) {
            Verdict::Accepted(_) => lo = m,
            v => (hi, past) = (m, v),
        }
    }
    (lo, past)
}

#[test]
fn a_call_may_take_20_kib() {
    let size = |n: usize| sim().external_call(PROBE, vec![0u8; n]).build().get_size();
    let n = 20_480 - (size(0) as usize);
    assert_eq!(size(n), 20_480);
    for f in [honest, forced] {
        let out = f(&mut sim(), vec![0u8; n]).included();
        out.assert_success();
        let too_big = InvalidTxError::TransactionSizeExceeded { size: 20_481, limit: 20_480 };
        assert_eq!(f(&mut sim(), vec![0u8; n + 1]).assert_refused(), Failure::InvalidTx(too_big));
    }
}

#[test]
fn a_run_may_burn_20_tgas_its_fees_included() {
    let (n, past) = boundary(sim, |n| op(14, &n.to_le_bytes()), 0, 1 << 30);
    assert_eq!(past, Verdict::Refused(Failure::GasLimitExceeded));
    let burnt = |n: u64| match decided(sim, op(14, &n.to_le_bytes())) {
        Verdict::Accepted(g) => g.as_gas(),
        v => panic!("{v:?}"),
    };
    // The fees before the run count toward the 20 Tgas: what the call
    // burnt in all, fees included, is within two loop turns of it.
    let (last, turn) = (burnt(n), burnt(n) - burnt(n - 1));
    assert!(last <= 20_000_000_000_000 && last + 2 * turn > 20_000_000_000_000, "{last} {turn}");
}

#[test]
fn a_run_may_record_50_kib_and_a_remove_counts_2000_bytes() {
    // Removes of missing keys record nothing but the 2,000 bytes each counts:
    // 25 make 50,000 bytes, under 51,200; 26 make 52,000.
    let (n, past) = boundary(sim, |n| u32_op(16, n as u32), 0, 1_000);
    assert_eq!((n, past), (25, host(RECORDED)));
}

#[test]
fn a_run_may_read_50_kib() {
    // 200 values of 1,000 bytes, written by an ordinary call.
    let stocked = || {
        let mut s = sim();
        let mut p = u32_op(18, 200);
        p.extend(1_000u32.to_le_bytes());
        s.call("alice.near", PROBE, "call")
            .args(p)
            .gas(Gas::from_teragas(300))
            .transact()
            .assert_success();
        s
    };
    let (n, past) = boundary(stocked, |n| u32_op(15, n as u32), 0, 200);
    assert_eq!(past, host(RECORDED));
    // Each read records its 1,000-byte value and its trie nodes.
    assert!((30..=51).contains(&n), "{n}");
}

#[test]
fn before_protocol_89_there_are_no_external_calls() {
    for f in [honest, forced] {
        let mut s = sim();
        s.config.protocol_version = 88;
        let want = Failure::InvalidTx(InvalidTxError::InvalidTransactionVersion);
        assert_eq!(f(&mut s, vec![]).assert_refused(), want);
    }
}

#[test]
fn a_contract_that_cannot_pay_the_fees_is_refused_before_the_pool() {
    let mut s = sim();
    s.set_balance(PROBE, Balance::from_yoctonear(1));
    let f = honest(&mut s, vec![]).assert_refused();
    assert!(matches!(f, Failure::InvalidTx(InvalidTxError::NotEnoughBalance { .. })), "{f:?}");
}

#[test]
fn a_call_to_an_account_without_a_contract_is_refused() {
    let mut s = sim();
    for to in ["alice.near", "nobody.near"] {
        let contract_id: AccountId = to.parse().unwrap();
        let out = s.external_call(to, vec![]).transact();
        assert_eq!(
            out.assert_refused(),
            Failure::InvalidTx(InvalidTxError::ContractNotFound { contract_id })
        );
    }
}

// ---- state init (NEP-655) ----

/// alice publishes the probe by hash; `wallet` is the universal account a
/// state init running it derives to, funded by a transfer and so not yet
/// initialized.
fn wallet(code: bool) -> (Sim, AccountId, RawStateInit) {
    let mut s = sim();
    let wasm = wat("ecc_probe.wat");
    let hash = near_contract_sim::nearcore::near_primitives::hash::hash(&wasm);
    s.tx("alice.near", "alice.near")
        .deploy_global(wasm, GlobalContractDeployMode::CodeHash)
        .transact()
        .assert_success();
    let init = UniversalStateInit::V1(UniversalStateInitV1 {
        code: code.then_some(GlobalContractIdentifier::CodeHash(hash)),
        data: BTreeMap::from([(b"seed".to_vec(), b"1".to_vec())]),
        access_keys: BTreeSet::new(),
    })
    .to_raw();
    let id = derive_universal_account_id(&init);
    s.tx("alice.near", &id).transfer(near(10)).transact().assert_success();
    (s, id, init)
}

#[test]
fn a_call_installs_its_state_init_on_an_uninitialized_universal_account() {
    let (mut s, id, init) = wallet(true);
    assert_eq!(s.account(&id).unwrap().global_contract_hash, None);
    let call = s.external_call(&id, op(0, b"hi")).state_init(init);
    let ecc = call.build();
    let out = call.transact().included();
    out.assert_success();
    assert_eq!(out.bytes(), b"hi");
    let height = out.transaction.as_ref().unwrap().block_height;
    assert_eq!(out.hash, create_ecc_outcome_id(ecc.hash_without_state_init(), height));
    assert!(s.account(&id).unwrap().global_contract_hash.is_some());
    assert_eq!(s.state(&id, b"seed").as_deref(), Some(&b"1"[..]));
    s.findings().assert_none();
}

#[test]
fn an_uninitialized_account_without_a_state_init_has_nothing_to_run() {
    let (mut s, id, _) = wallet(true);
    let contract_id = id.clone();
    let want = Failure::InvalidTx(InvalidTxError::ContractNotFound { contract_id });
    assert_eq!(s.external_call(&id, vec![]).transact().assert_refused(), want.clone());
    assert_eq!(s.external_call(&id, vec![]).forced().transact().included().assert_failure(), want);
}

#[test]
fn a_state_init_without_code_has_nothing_to_run() {
    let (mut s, id, init) = wallet(false);
    let contract_id = id.clone();
    let want = Failure::InvalidTx(InvalidTxError::ContractNotFound { contract_id });
    let out = s.external_call(&id, vec![]).state_init(init.clone()).transact();
    assert_eq!(out.assert_refused(), want.clone());
    let out = s.external_call(&id, vec![]).state_init(init).forced().transact().included();
    assert_eq!(out.assert_failure(), want);
    assert_eq!(s.state(&id, b"seed"), None);
}

#[test]
fn a_refused_call_rolls_its_install_back() {
    for f in [false, true] {
        let (mut s, id, init) = wallet(true);
        let before = s.balance(&id);
        let call = s.external_call(&id, vec![1]).state_init(init);
        let out = if f { call.forced() } else { call }.transact();
        match f {
            false => assert_eq!(out.assert_refused(), Failure::Panic("refused".into())),
            true => assert_eq!(out.included().assert_failure(), Failure::Panic("refused".into())),
        }
        assert_eq!(s.balance(&id), before);
        assert_eq!(s.account(&id).unwrap().global_contract_hash, None);
        assert_eq!(s.state(&id, b"seed"), None);
    }
}

#[test]
fn an_initialized_account_ignores_a_state_init_and_pays_as_without_it() {
    let (mut s, id, init) = wallet(true);
    s.external_call(&id, vec![]).state_init(init.clone()).transact().included().assert_success();
    s.set_state(&id, b"seed", Some(b"2"));
    let mut twin = s.clone();
    let with = s.external_call(&id, vec![0]).state_init(init).transact().included();
    let without = twin.external_call(&id, vec![0]).transact().included();
    with.assert_success();
    assert_eq!(with.hash, without.hash);
    assert_eq!(with.tokens_burnt(), without.tokens_burnt());
    assert_eq!(s.state(&id, b"seed").as_deref(), Some(&b"2"[..]));
}

// ---- the node: pool, packing, repeats ----

#[test]
fn a_submitted_call_waits_in_the_pool_for_a_block() {
    let mut s = sim();
    let sent = s.external_call(PROBE, vec![]).submit();
    assert!(matches!(s.external_call_outcome(sent), ExternalCallOutcome::Pending));
    assert!(!s.is_idle());
    s.step();
    s.external_call_outcome(sent).included().assert_success();
}

#[test]
fn the_same_call_sent_twice_before_a_block_is_one_send() {
    let mut s = sim();
    let a = s.external_call(PROBE, vec![2, b'x']).submit();
    let b = s.external_call(PROBE, vec![2, b'x']).submit();
    assert_eq!(a, b);
    s.settle();
    s.external_call_outcome(a).included().assert_success();
}

#[test]
fn a_chunk_holds_one_call_per_contract() {
    let mut s = sim();
    let first = s.external_call(PROBE, op(2, b"1")).submit();
    let second = s.external_call(PROBE, op(2, b"2")).submit();
    let bob = s.external_call("bob.near", vec![]).submit();
    s.step();
    let h = s.height();
    let at = |s: &Sim, c| s.external_call_outcome(c).included().transaction.unwrap().block_height;
    assert_eq!((at(&s, first), at(&s, bob)), (h, h));
    assert!(matches!(s.external_call_outcome(second), ExternalCallOutcome::Pending));
    s.step();
    assert_eq!(at(&s, second), h + 1);
    assert_eq!(s.state(PROBE, b"k").as_deref(), Some(&b"2"[..]));
}

#[test]
fn a_resend_of_an_accepted_call_within_1000_blocks_is_dropped_as_a_repeat() {
    let mut s = sim();
    let first = honest(&mut s, op(2, b"once")).included();
    let at = first.transaction.as_ref().unwrap().block_height;
    // A node drops it while the inclusion is at most 1,000 blocks below
    // the final head, here the last block.
    s.advance_to(at + 1_000);
    let before = s.balance(PROBE);
    match honest(&mut s, op(2, b"once")) {
        ExternalCallOutcome::Repeat(o) => assert_eq!(o.hash, first.hash),
        other => panic!("expected a repeat, got {other}"),
    }
    assert_eq!(s.balance(PROBE), before);
    // A block later the window has passed.
    s.advance_to(at + 1_001);
    let again = honest(&mut s, op(2, b"once")).included();
    assert_ne!(again.hash, first.hash);
}

/// A transaction's receipt held until a receipt down an external call's
/// chain runs (`Receipts::in_tx`, by the call's unique id): alice's
/// `ping` of bob, which runs alongside the call's third hop plain, runs a
/// block after it.
#[test]
fn a_hold_can_wait_for_a_receipt_of_an_external_call() {
    let run = |hold: bool| {
        let mut s = sim();
        s.create_account("carol.near", near(10));
        let hop = |next: &str| Reply::call("carol.near", next, ()).then(Reply::json("hop"));
        let stub = Stub::new().on("hop1", hop("hop2")).on("hop2", hop("hop3"));
        s.deploy_stub("carol.near", stub.on("hop3", Reply::json("done")));
        let hop1 = promise(Gas::from_teragas(60), 0, Balance::ZERO, "hop1", "carol.near");
        let sent = s.external_call(PROBE, hop1).submit();
        s.step();
        let unique = s.external_call_outcome(sent).included().hash;
        let ping = promise(Gas::from_teragas(30), 0, Balance::ZERO, "ping", "bob.near");
        let mut tx = s.call("alice.near", PROBE, "call").args(ping).gas(Gas::from_teragas(100));
        if hold {
            let until = Receipts::method("hop3").in_tx(unique);
            tx = tx.inject(Receipts::method("ping"), Fault::HoldUntil(until));
        }
        let out = tx.transact();
        let hop3 = s.outcome(unique).expect_receipt("hop3").block_height;
        let ping = out.expect_receipt("ping");
        (ping.block_height - hop3, ping.held.is_some())
    };
    assert_eq!(run(false), (0, false));
    assert_eq!(run(true), (1, true));
}

#[test]
fn a_call_refused_in_its_chunk_can_be_retried() {
    let mut s = sim();
    let first = forced(&mut s, vec![1]).included();
    first.assert_failure();
    let again = forced(&mut s, vec![1]).included();
    assert_ne!(again.hash, first.hash);
}

#[test]
fn a_call_sees_its_contracts_balance_after_the_chunks_signed_transactions() {
    let mut s = sim();
    s.tx(PROBE, "alice.near").transfer(near(1)).submit();
    let sent = s.external_call(PROBE, vec![12]).submit();
    s.step();
    let out = s.external_call_outcome(sent).included();
    let seen = le_u128(&out);
    let charge = out.transaction.as_ref().unwrap().tokens_burnt.as_yoctonear();
    assert_eq!(seen, s.balance(PROBE).as_yoctonear() + charge);
    assert!(seen < near(99).as_yoctonear());
}

#[test]
fn an_external_call_is_rendered_as_one() {
    let out = honest(&mut sim(), vec![]).included();
    let text = out.to_string();
    let head = text.lines().next().unwrap();
    assert!(head.starts_with("external call ") && head.contains(" to probe.near at #"), "{text}");
    assert!(text.contains("external call probe.near"), "{text}");
}

#[test]
#[should_panic(expected = "a rewind (set_height) that includes it at that height again")]
fn a_rewind_that_includes_the_same_call_at_the_same_height_panics() {
    let mut s = sim();
    s.set_height(50);
    forced(&mut s, vec![1]).included().assert_failure();
    s.set_height(50);
    forced(&mut s, vec![1]);
}

// ---- views ----

#[test]
fn a_view_of_the_reserved_method_is_the_dry_run_on_the_last_block() {
    let mut s = sim();
    let before = s.account(PROBE).unwrap();
    let v = s.view_raw(PROBE, "__external_call", op(0, b"hi"));
    assert_eq!(v.bytes(), b"hi");
    assert_eq!(v.logs(), ["accepted"]);
    // It writes and charges nothing.
    s.view_raw(PROBE, "__external_call", op(2, b"v")).assert_success();
    assert_eq!(s.state(PROBE, b"k"), None);
    assert_eq!(s.account(PROBE).unwrap(), before);
    // It runs as a call in the next chunk would: on top of the last block.
    let v = s.view_raw(PROBE, "__external_call", vec![17]);
    let index = u64::from_le_bytes(v.bytes()[..8].try_into().unwrap());
    let ns = u64::from_le_bytes(v.bytes()[8..].try_into().unwrap());
    assert_eq!((index, ns), (s.height(), s.timestamp_ns()));
    // A refusal is the view's failure, with what the run logged.
    let v = s.view_raw(PROBE, "__external_call", vec![19]);
    assert_eq!(v.assert_failure(), Failure::Panic("refused".into()));
    assert_eq!(v.logs(), ["accepted"]);
    let v = s.view_raw(PROBE, "__external_call", vec![3]);
    assert_eq!(v.assert_failure(), Failure::Host(format!("signer_account_pk {NOT_ALLOWED}")));
    let contract_id: AccountId = "alice.near".parse().unwrap();
    let v = s.view_raw("alice.near", "__external_call", vec![]);
    assert_eq!(
        v.assert_failure(),
        Failure::InvalidTx(InvalidTxError::ContractNotFound { contract_id })
    );
    // It agrees with the call sent.
    assert_eq!(honest(&mut s, op(0, b"hi")).included().bytes(), b"hi");
}
