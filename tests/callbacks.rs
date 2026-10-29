//! `Tx::promise_results`: a callback run on promise results the test picks,
//! compared with the same callback reached through real promises, and with
//! the same callback whose promises `Tx::inject` fails or answers. And
//! `Tx::predecessor`: a call delivered as one from another account.

mod common;
use common::*;
use near_contract_sim::PromiseResult::{Failed, Successful};
use near_contract_sim::{
    ActionErrorKind, Balance, Failure, Fault, Gas, GlobalContractDeployMode,
    GlobalContractIdentifier, PromiseResult, ReceiptOutcome, Receipts, Reply, Sim, TxOutcome,
};
use serde_json::json;
use std::panic::{AssertUnwindSafe, catch_unwind};

fn callback_probe_wasm() -> Vec<u8> {
    wat::parse_file(manifest_dir().join("tests/res/callback_probe.wat")).unwrap()
}

/// alice.near, and the callback probe at probe.near.
fn callback_probe_sim() -> Sim {
    let mut sim = sim_with_users();
    sim.deploy("probe.near", callback_probe_wasm());
    sim
}

/// The probe's `cb` on `results`, with `args` and 20 Tgas, as `promise_then`
/// attaches it in `start`.
fn inject(
    sim: &mut Sim,
    args: &[u8],
    results: impl IntoIterator<Item = PromiseResult>,
) -> TxOutcome {
    sim.call("alice.near", "probe.near", "cb")
        .args(args)
        .gas(Gas::from_teragas(20))
        .promise_results(results)
        .transact()
}

/// What the probe's `cb` saw, parsed.
#[derive(Debug, PartialEq, Eq)]
struct Seen {
    signer: String,
    signer_key: Vec<u8>,
    predecessor: String,
    current: String,
    input: (u64, Vec<u8>),
    deposit: u128,
    prepaid_gas: u64,
    /// Each result: `None` if it failed, else its length and sha256.
    results: Vec<Option<(u64, Vec<u8>)>>,
}

fn seen(r: &ReceiptOutcome) -> Seen {
    let v = r.value().unwrap_or_else(|| panic!("cb returned nothing: {:?}", r.status));
    let mut t = Reader(v);
    let text = |b: Vec<u8>| String::from_utf8(b).unwrap();
    let s = Seen {
        signer: text(t.bytes()),
        signer_key: t.bytes(),
        predecessor: text(t.bytes()),
        current: text(t.bytes()),
        input: t.digest(),
        deposit: u128::from_le_bytes(t.take(16).try_into().unwrap()),
        prepaid_gas: t.u64(),
        results: (0..t.u64())
            .map(|_| match t.u64() {
                1 => Some(t.digest()),
                2 => None,
                s => panic!("status {s}"),
            })
            .collect(),
    };
    assert!(t.0.is_empty());
    s
}

struct Reader<'a>(&'a [u8]);
impl Reader<'_> {
    fn take(&mut self, n: usize) -> Vec<u8> {
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        head.to_vec()
    }
    fn u64(&mut self) -> u64 {
        u64::from_le_bytes(self.take(8).try_into().unwrap())
    }
    fn bytes(&mut self) -> Vec<u8> {
        let n = self.u64() as usize;
        self.take(n)
    }
    fn digest(&mut self) -> (u64, Vec<u8>) {
        (self.u64(), self.take(32))
    }
}

/// The same callback, reached through `promise_then` and delivered with the
/// results it got there, sees the same context and burns the same gas, to
/// the unit. Only where it sits differs: its id, its block, and that the
/// tree marks it delivered.
#[test]
fn a_delivered_callback_sees_what_a_real_one_sees() {
    let base = callback_probe_sim();
    let hello = b"hello".to_vec();
    for (start, results) in [
        ("start", vec![Successful(hello.clone())]),
        ("start_and", vec![Successful(hello.clone()), Failed]),
    ] {
        let mut real = base.clone();
        let out = real.call("alice.near", "probe.near", start).args(hello.clone()).transact();
        let want = out.expect_receipt("cb");
        let mut sim = base.clone();
        let got = inject(&mut sim, &hello, results);
        let got = got.expect_receipt("cb");
        assert_eq!(seen(got), seen(want));
        let s = seen(got);
        assert_eq!((s.signer.as_str(), s.predecessor.as_str()), ("alice.near", "probe.near"));
        assert_eq!(s.prepaid_gas, Gas::from_teragas(20).as_gas());
        assert_eq!(
            (got.gas_burnt, &got.gas_profile, got.prepaid_exec_fees, got.headroom()),
            (want.gas_burnt, &want.gas_profile, want.prepaid_exec_fees, want.headroom())
        );
        assert_eq!(got.predecessor_id, want.predecessor_id);
        assert!(got.delivered && !want.delivered);
        assert!(got.is_callback() && want.is_callback(), "both wait on results");
        real.findings().assert_none();
        sim.findings().assert_none();
    }
}

/// A callback whose promises a fault fails (`PromiseFailed`) or answers
/// (`ReturnValue`) runs where `promise_then` put it, and sees what the same
/// callback delivered with those results sees, burning the same gas. A
/// result that failed stays failed under `ReturnValue`.
#[test]
fn a_faulted_callback_sees_what_a_delivered_one_sees() {
    let base = callback_probe_sim();
    let (hello, world) = (b"hello".to_vec(), b"world!".to_vec());
    let answer = Fault::ReturnValue(world.clone());
    for (start, fault, results) in [
        ("start", Fault::PromiseFailed, vec![Failed]),
        ("start_and", Fault::PromiseFailed, vec![Failed, Failed]),
        ("start", answer.clone(), vec![Successful(world.clone())]),
        ("start_and", answer, vec![Successful(world.clone()), Failed]),
    ] {
        let mut sim = base.clone();
        let out = sim
            .call("alice.near", "probe.near", start)
            .args(hello.clone())
            .inject(Receipts::callback("cb"), fault.clone())
            .transact();
        let got = out.expect_receipt("cb");
        let want = inject(&mut base.clone(), &hello, results);
        let want = want.expect_receipt("cb");
        assert_eq!(seen(got), seen(want), "{start} {fault}");
        assert_eq!((got.gas_burnt, &got.fault), (want.gas_burnt, &Some(fault)));
        assert!(!got.delivered && got.is_callback());
        let failed: Vec<_> = out.receipts.iter().filter(|r| r.failure().is_some()).collect();
        assert!(failed.iter().all(|r| !r.unhandled), "{out}");
    }
}

/// ft.near and r.near as `ft_transfer_call` leaves them once r.near's
/// `ft_on_transfer` has answered `"40"`: r.near holds the 100 alice.near
/// sent, and its stub is set to answer `"40"`.
fn sent() -> Sim {
    let mut sim = ft_sim();
    sim.stub_reply("r.near", "ft_on_transfer", Reply::json("40"));
    sim.call("alice.near", "ft.near", "ft_transfer")
        .args_json(json!({"receiver_id": "r.near", "amount": "100"}))
        .deposit(Balance::from_yoctonear(1))
        .transact()
        .assert_success();
    sim
}

/// The args near-sdk's token gives its resolve, in its order.
const RESOLVE_ARGS: &str = r#"{"sender_id":"alice.near","receiver_id":"r.near","amount":"100"}"#;

/// near-sdk's `ft_resolve_transfer` delivered `"40"` is the one
/// `ft_transfer_call` reaches when the receiver answers `"40"`: same value,
/// logs, balances and gas, to the unit, trie-node gas included, since the
/// trie holds the same keys.
#[test]
fn ft_resolve_transfer_delivered_matches_one_reached_through_ft_transfer_call() {
    let mut real = ft_sim();
    real.stub_reply("r.near", "ft_on_transfer", Reply::json("40"));
    let out = real
        .call("alice.near", "ft.near", "ft_transfer_call")
        .args_json(json!({"receiver_id": "r.near", "amount": "100", "msg": ""}))
        .deposit(Balance::from_yoctonear(1))
        .gas(Gas::from_teragas(100))
        .transact();
    let want = out.expect_receipt("ft_resolve_transfer");

    let mut sim = sent();
    let alice = sim.balance("alice.near");
    let out = sim
        .call("alice.near", "ft.near", "ft_resolve_transfer")
        .args(RESOLVE_ARGS)
        .gas(want.attached_gas)
        .promise_results([PromiseResult::json("40")])
        .transact();
    let got = out.expect_receipt("ft_resolve_transfer");
    assert_eq!((got.value(), &got.logs), (want.value(), &want.logs));
    assert_eq!((got.gas_burnt, &got.gas_profile), (want.gas_burnt, &want.gas_profile));
    for id in ["alice.near", "r.near"] {
        assert_eq!(ft_balance(&sim, id), ft_balance(&real, id));
    }
    // The signer bought the gas: it paid what the transaction burnt.
    assert_eq!(alice.checked_sub(sim.balance("alice.near")), Some(out.tokens_burnt()));
    sim.findings().assert_none();
    real.findings().assert_none();
}

/// Callback totality, on results: `ft_resolve_transfer`
/// settles any result a receiver can give, and two only the helper can.
#[test]
fn ft_resolve_transfer_on_any_result() {
    let sent = sent(); // alice.near sent r.near 100 with ft_transfer
    let args = r#"{"sender_id":"alice.near","receiver_id":"r.near","amount":"100"}"#;
    // The other shapes: `ft_resolve_transfer_settles_each_result`.
    let mut sim = sent.clone();
    let out = sim
        .call("alice.near", "ft.near", "ft_resolve_transfer")
        .args(args)
        .gas(Gas::from_teragas(5))
        .promise_results([Successful(vec![b'7'; 4 << 20])]) // max_length_returned_data
        .transact();
    assert!(out.expect_receipt("ft_resolve_transfer").is_success(), "{out}");
    // With none, it fails: the token never schedules it that way.
    let mut sim = sent.clone();
    let out = sim
        .call("alice.near", "ft.near", "ft_resolve_transfer")
        .args(args)
        .gas(Gas::from_teragas(5))
        .promise_results([])
        .transact();
    let f = out.expect_receipt("ft_resolve_transfer").failure();
    assert_eq!(f, Some(Failure::Host("Accessed invalid promise result index: 0".into())));
    assert_eq!(ft_balance(&sim, "r.near"), "100");
}

/// Each result shape settles as the token's rule says: keep what the
/// receiver used, at most what was sent; refund all on a failure or an
/// unreadable answer.
#[test]
fn ft_resolve_transfer_settles_each_result() {
    let base = sent();
    let max = PromiseResult::json(u128::MAX.to_string());
    let cases = [
        (vec![PromiseResult::json("0")], "100"),
        (vec![PromiseResult::json("40")], "60"),
        (vec![PromiseResult::json("100")], "0"),
        (vec![PromiseResult::json("150")], "0"),
        (vec![max], "0"),
        (vec![Successful(vec![])], "0"),
        (vec![Successful(b"{".to_vec())], "0"),
        (vec![Failed], "0"),
        (vec![PromiseResult::json("40"), Failed], "60"),
        (vec![Failed, PromiseResult::json("40")], "0"),
    ];
    for (results, kept) in cases {
        let mut sim = base.clone();
        let out = sim
            .call("alice.near", "ft.near", "ft_resolve_transfer")
            .args(RESOLVE_ARGS)
            .gas(Gas::from_teragas(5))
            .promise_results(results)
            .transact();
        out.assert_success();
        assert_eq!(ft_balance(&sim, "r.near"), kept, "{out}");
        sim.findings().assert_none();
    }
}

/// The tree marks the callback delivered, and its gas table and logs are
/// any receipt's.
#[test]
fn the_tree_marks_a_delivered_callback() {
    let mut sim = sent();
    let out = sim
        .call("alice.near", "ft.near", "ft_resolve_transfer")
        .args(RESOLVE_ARGS)
        .gas(Gas::from_teragas(5))
        .promise_results([PromiseResult::json("40")])
        .transact();
    assert_rendered(
        &out.to_string(),
        r#"tx 5iv9ir… signed by alice.near at #14 (conversion 0.31 Tgas) -> SUCCESS
└─ #15 ft.near::ft_resolve_transfer [callback of 1 delivered result]  attached 5.00 Tgas, burnt 1.68 Tgas (0.79 + 0.89 exec fees), headroom 4.21 Tgas  ok -> "60"
      log: EVENT_JSON:{"standard":"nep141","version":"1.0.0","event":"ft_transfer","data":[{"old_owner_id":"r.near","new_owner_id":"alice.near","amount":"40","memo":"refund"}]}
(1 refund receipt(s) not shown)

gas (Tgas)                          wasm    load storage    host    send    exec   total
conversion alice.near                  -       -       -       -   0.312       -   0.312
#15 ft.near::ft_resolve_transfer   0.050   0.135   0.431   0.175       -   0.888   1.679
refunds (not charged)                  -       -       -       -       -       -   0.223
total 2.21 Tgas (2214699529258 gas)
"#,
    );
}

/// A result can be as long as a receipt can return, 4 MiB; a byte more
/// panics at submit, since no receipt returns it. Results longer together
/// than nearcore#16561's input limit, 4,194,944 bytes, run: nearcore 2.14.0
/// enforces that limit from nightly protocol 157 only (nearcore#16575), and
/// the simulator runs 84 to 88. At 87 and 88 the limit is `u32::MAX`, past
/// what 128 inputs of 4 MiB can reach.
#[test]
fn results_up_to_the_return_size_limit() {
    let base = callback_probe_sim();
    let max = base.config.runtime_config().wasm_config.limit_config.max_length_returned_data;
    let max = max as usize;
    let total = 4_194_944;
    let cb = |sim: &mut Sim, results: Vec<PromiseResult>| {
        sim.call("alice.near", "probe.near", "cb")
            .gas(Gas::from_teragas(300))
            .promise_results(results)
            .transact()
    };
    let mut sim = base.clone();
    let out = cb(&mut sim, vec![Successful(vec![7; max])]);
    let s = seen(out.expect_receipt("cb"));
    assert_eq!(s.results[0].as_ref().map(|r| r.0), Some(max as u64));
    sim.findings().assert_none();

    let mut sim = base.clone();
    let p = catch_unwind(AssertUnwindSafe(|| cb(&mut sim, vec![Successful(vec![7; max + 1])])));
    assert_eq!(
        panic_message(p),
        "a promise result of 4194305 bytes: no receipt returns more than \
         max_length_returned_data, 4194304"
    );
    assert!(sim.is_idle(), "nothing was submitted");

    // nearcore counts each result as stored, 5 bytes more than its value:
    // these two are a byte past that limit, and run at 88 and at 87.
    let two = |sim: &mut Sim, n| cb(sim, vec![Successful(vec![7; max]), Successful(vec![7; n])]);
    for version in [88, 87] {
        let mut sim = base.clone();
        sim.config.protocol_version = version;
        let limits = &sim.config.runtime_config().wasm_config.limit_config;
        assert_eq!(limits.max_receipt_total_input_size, u64::from(u32::MAX));
        let out = two(&mut sim, total - max - 9);
        assert!(out.expect_receipt("cb").is_success(), "{out}");
        sim.findings().assert_none();
    }
}

/// The attached gas is the call's, up to the 1 PGas a transaction can
/// attach; past it the transaction is refused, and nothing is left to
/// deliver.
#[test]
fn gas_up_to_the_prepaid_cap() {
    let mut sim = callback_probe_sim();
    let pgas = Gas::from_teragas(1_000);
    let cb = |sim: &mut Sim, gas| {
        sim.call("alice.near", "probe.near", "cb").gas(gas).promise_results([Failed]).transact()
    };
    let out = cb(&mut sim, pgas);
    assert_eq!(seen(out.expect_receipt("cb")).prepaid_gas, pgas.as_gas());
    let out = cb(&mut sim, pgas.checked_add(Gas::from_gas(1)).unwrap());
    assert!(matches!(out.assert_failure(), Failure::InvalidTx(_)), "{out}");
    assert!(out.receipts.is_empty());
    let out = sim.call("alice.near", "probe.near", "answer").transact();
    assert!(!out.expect_receipt("answer").delivered, "{out}");
    sim.findings().assert_none();
}

/// What the callback schedules runs as usual, after it.
#[test]
fn a_delivered_callback_schedules_promises_that_run() {
    let mut sim = callback_probe_sim();
    let out = sim
        .call("alice.near", "probe.near", "relay")
        .args(b"hi")
        .gas(Gas::from_teragas(20))
        .promise_results([Failed])
        .transact();
    assert_eq!(out.value(), Some(&b"hi"[..]), "{out}");
    let answer = out.expect_receipt("answer");
    assert_eq!(answer.predecessor_id, "probe.near");
    assert!(out.expect_receipt("relay").delivered && !answer.delivered);
    assert!(answer.block_height > out.expect_receipt("relay").block_height);
    sim.findings().assert_none();
}

/// With no results, it is a call of the contract to itself, marked as one
/// delivered from it, as `.predecessor(receiver)` is.
#[test]
fn no_results_is_a_call_of_the_contract_to_itself() {
    let mut sim = callback_probe_sim();
    let out = inject(&mut sim, b"", []);
    let s = seen(out.expect_receipt("cb"));
    assert_eq!((s.predecessor.as_str(), s.results.len()), ("probe.near", 0));
    assert!(out.to_string().contains("─ #4 probe.near::cb [delivered from probe.near]  "));
    let out = sim.call("alice.near", "probe.near", "cb").predecessor("probe.near").transact();
    assert!(out.to_string().contains(" probe.near::cb [delivered from probe.near]  "), "{out}");
    sim.findings().assert_none();
}

/// Code a global contract provides runs too, on an account that uses it
/// and on a NEP-616 one whose id derives from its state init.
#[test]
fn global_contracts_and_deterministic_accounts() {
    use near_contract_sim::Action;
    use near_contract_sim::nearcore::near_primitives::action::DeterministicStateInitAction;
    use near_contract_sim::nearcore::near_primitives::deterministic_account_id::*;
    use near_contract_sim::nearcore::near_primitives::hash::hash;
    use near_contract_sim::nearcore::near_primitives::utils::derive_near_deterministic_account_id;
    let mut sim = sim_with_users();
    let wasm = callback_probe_wasm();
    let publish = sim
        .tx("bob.near", "bob.near")
        .deploy_global(wasm.clone(), GlobalContractDeployMode::CodeHash);
    publish.transact().assert_success();
    let code = GlobalContractIdentifier::CodeHash(hash(&wasm));
    sim.create_account("g.near", near(10));
    sim.tx("g.near", "g.near").use_global(code.clone()).transact().assert_success();
    let state_init = DeterministicAccountStateInit::V1(DeterministicAccountStateInitV1 {
        code,
        data: Default::default(),
    });
    let id = derive_near_deterministic_account_id(&state_init);
    let init = DeterministicStateInitAction { state_init, deposit: near(1) };
    sim.tx("alice.near", &id)
        .action(Action::DeterministicStateInit(Box::new(init)))
        .transact()
        .assert_success();
    for contract in ["g.near", id.as_str()] {
        let out = sim
            .call("alice.near", contract, "cb")
            .gas(Gas::from_teragas(20))
            .promise_results([Successful(b"x".to_vec())])
            .transact();
        let s = seen(out.expect_receipt("cb"));
        assert_eq!((s.predecessor.as_str(), s.current.as_str()), (contract, contract));
        assert_eq!(s.results.len(), 1);
    }
    sim.findings().assert_none();
}

/// Only one call to another account than the signer can be delivered, and
/// not through a relayer; a refused one submits nothing.
#[test]
fn only_one_call_to_another_account_can_be_delivered() {
    let mut sim = callback_probe_sim();
    let mut refused = |f: &dyn Fn(&mut Sim)| {
        let p = catch_unwind(AssertUnwindSafe(|| f(&mut sim)));
        assert!(sim.is_idle());
        panic_message(p)
    };
    let to_itself = refused(&|sim| {
        sim.call("probe.near", "probe.near", "cb").promise_results([Failed]).submit();
    });
    assert!(
        to_itself
            .starts_with("Tx::promise_results needs a signer other than the receiver, probe.near"),
        "{to_itself}"
    );
    let two_calls = refused(&|sim| {
        sim.call("alice.near", "probe.near", "cb").call("cb").promise_results([]).submit();
    });
    assert_eq!(two_calls, "Tx::promise_results needs the transaction to be one call");
    let transfer = refused(&|sim| {
        sim.tx("alice.near", "probe.near").transfer(near(1)).promise_results([]).submit();
    });
    assert_eq!(transfer, two_calls);
    let relayed = refused(&|sim| {
        let _ = sim.call("alice.near", "probe.near", "cb").promise_results([]).relay("bob.near");
    });
    assert_eq!(
        relayed,
        "the simulator can't deliver a delegate as a callback or from a predecessor"
    );
}

/// The vault guards `ft_on_transfer` with "only the vault's token": a call
/// delivered from the token passes it and one from any other account is
/// refused, each in one line, with no token deployed. The receipt keeps
/// alice as its signer, the tree marks it, and its failure is the
/// transaction's, so nothing is left for `findings`.
#[test]
fn a_call_from_another_predecessor_passes_or_fails_its_guard() {
    let mut sim = sim_with_users();
    sim.deploy("vault.near", res("vault.wasm"));
    sim.call("vault.near", "vault.near", "new")
        .args_json(json!({ "token_id": "token.near" }))
        .transact()
        .assert_success();
    let deposit = |sim: &mut Sim, from: &str| {
        sim.call("alice.near", "vault.near", "ft_on_transfer")
            .args_json(json!({ "sender_id": "alice.near", "amount": "100", "msg": "" }))
            .predecessor(from)
            .transact()
    };
    let out = deposit(&mut sim, "token.near");
    assert_eq!(out.json::<String>(), "0");
    let r = &out.receipts[0];
    assert_eq!((r.predecessor_id.as_str(), r.delivered), ("token.near", true));
    assert_eq!(out.transaction.as_ref().unwrap().receiver_id, "alice.near");
    assert!(
        out.to_string().contains("vault.near::ft_on_transfer [delivered from token.near]"),
        "{out}"
    );
    let shares = sim.view("vault.near", "shares_of", json!({ "account_id": "alice.near" }));
    assert_eq!(shares.json::<String>(), "100");
    let out = deposit(&mut sim, "evil.near");
    assert_eq!(out.assert_failure(), Failure::Panic("only the vault's token deposits".into()));
    sim.findings().assert_none();
    assert!(sim.account("token.near").is_none() && sim.account("evil.near").is_none());
}

/// A failed call's deposit goes back to the signer, who paid it, not to
/// the predecessor, which needn't exist: alice loses the same with a
/// deposit as without.
#[test]
fn a_failed_deposit_from_another_predecessor_goes_back_to_the_signer() {
    let base = callback_probe_sim();
    let lost = |deposit| {
        let mut sim = base.clone();
        let before = sim.balance("alice.near");
        let out = sim
            .call("alice.near", "probe.near", "boom")
            .deposit(deposit)
            .predecessor("ghost.near")
            .transact();
        assert_eq!(out.assert_failure(), Failure::Panic("boom".into()));
        sim.findings().assert_none();
        assert!(sim.account("ghost.near").is_none());
        before.checked_sub(sim.balance("alice.near")).unwrap()
    };
    assert_eq!(lost(near(1)), lost(Balance::ZERO));
}

/// A call to the signer itself runs before it can be delivered, a delegate
/// carries nothing but its actions, and promise results come from the
/// receiver: each refused at submit, which submits nothing.
#[test]
fn a_predecessor_is_refused_where_it_cant_be_delivered() {
    let mut sim = callback_probe_sim();
    let mut refused = |f: &dyn Fn(&mut Sim)| {
        let p = catch_unwind(AssertUnwindSafe(|| f(&mut sim)));
        assert!(sim.is_idle());
        panic_message(p)
    };
    let to_itself = refused(&|sim| {
        sim.call("probe.near", "probe.near", "cb").predecessor("x.near").submit();
    });
    assert!(
        to_itself.starts_with("Tx::predecessor needs a signer other than the receiver"),
        "{to_itself}"
    );
    let both = refused(&|sim| {
        sim.call("alice.near", "probe.near", "cb")
            .promise_results([])
            .predecessor("x.near")
            .submit();
    });
    assert!(both.starts_with("Tx::promise_results delivers the call from its receiver"), "{both}");
    let relayed = refused(&|sim| {
        let _ = sim.call("alice.near", "probe.near", "cb").predecessor("x.near").relay("bob.near");
    });
    assert!(relayed.starts_with("the simulator can't deliver a delegate"), "{relayed}");
    // The hash doesn't cover the predecessor: a copy must name the same.
    fn call<'a>(sim: &'a mut Sim, from: &str) -> near_contract_sim::Tx<'a> {
        sim.call("alice.near", "probe.near", "cb").nonce(7).predecessor(from)
    }
    let hash = call(&mut sim, "x.near").submit();
    assert_eq!(call(&mut sim, "x.near").submit(), hash);
    let other = panic_message(catch_unwind(AssertUnwindSafe(|| call(&mut sim, "y.near").submit())));
    assert!(other.contains("with other promise results or another predecessor"), "{other}");
}

/// A deposit comes from the signer, and a failure refunds it to the
/// contract, the receipt's predecessor. The failure is the transaction's
/// result, so `check` doesn't flag it.
#[test]
fn a_deposit_comes_from_the_signer_and_a_failure_refunds_the_contract() {
    let base = callback_probe_sim();
    let gained = |deposit| {
        let mut sim = base.clone();
        let before = sim.balance("probe.near");
        let out = sim
            .call("alice.near", "probe.near", "boom")
            .deposit(deposit)
            .gas(Gas::from_teragas(20))
            .promise_results([Failed])
            .transact();
        assert_eq!(out.assert_failure(), Failure::Panic("boom".into()));
        sim.findings().assert_none();
        sim.balance("probe.near").checked_sub(before).unwrap()
    };
    assert_eq!(gained(near(1)).checked_sub(gained(Balance::ZERO)), Some(near(1)));
}

/// On the oldest protocol the pinned nearcore runs too, a delivered callback
/// matches one reached through `promise_then`, its receipt's size included.
#[test]
fn a_delivered_callback_matches_on_protocol_84() {
    let mut base = callback_probe_sim();
    base.config.protocol_version = 84;
    let mut real = base.clone();
    let out = real.call("alice.near", "probe.near", "start").args(b"hello").transact();
    let want = out.expect_receipt("cb");
    let mut sim = base.clone();
    let out = inject(&mut sim, b"hello", [Successful(b"hello".to_vec())]);
    let got = out.expect_receipt("cb");
    assert_eq!(seen(got), seen(want));
    assert_eq!((got.gas_burnt, &got.gas_profile), (want.gas_burnt, &want.gas_profile));
    sim.findings().assert_none();
}

/// The callback runs in the block after its conversion, after the receipts
/// of the transactions that converted before it: one of those can change
/// the state it reads while it is in flight.
#[test]
fn a_transaction_ahead_changes_what_the_callback_reads() {
    let mut sim = sent();
    sim.call("r.near", "ft.near", "ft_transfer")
        .args_json(json!({"receiver_id": "alice.near", "amount": "100"}))
        .deposit(Balance::from_yoctonear(1))
        .submit();
    let hash = sim
        .call("alice.near", "ft.near", "ft_resolve_transfer")
        .args(RESOLVE_ARGS)
        .gas(Gas::from_teragas(5))
        .promise_results([PromiseResult::json("100")])
        .submit();
    sim.settle();
    // r.near returned the 100 first, so there was nothing left to refund.
    let out = sim.outcome(hash);
    assert_eq!(out.json::<String>(), "100");
    assert_eq!(
        (ft_balance(&sim, "alice.near"), ft_balance(&sim, "r.near")),
        ("1000".into(), "0".into())
    );
    sim.findings().assert_none();
}

/// A contract imported from state records, with no key of its own or one
/// the `Sim` holds, takes a delivered callback: the signer is another
/// account. It still has no key after.
#[test]
fn a_keyless_imported_contract_takes_a_delivered_callback() {
    use near_contract_sim::StateRecord;
    let records = callback_probe_sim().state_records(["probe.near"]);
    let keyless = records.into_iter().filter(|r| !matches!(r, StateRecord::AccessKey { .. }));
    let mut sim = sim_with_users();
    sim.apply_state_records(keyless);
    assert!(sim.access_keys("probe.near").is_empty());
    let out = inject(&mut sim, b"hello", [Successful(b"hello".to_vec())]);
    assert_eq!(seen(out.expect_receipt("cb")).predecessor, "probe.near");
    assert!(sim.access_keys("probe.near").is_empty());
    sim.findings().assert_none();
}

/// A contract whose balance fell below its storage stake while its
/// callback was in flight fails it with `LackBalanceForState`, and a
/// delivered callback fails the same way, with the same gas.
#[test]
fn below_the_storage_stake_a_delivered_callback_fails_as_a_real_one() {
    let base = callback_probe_sim();
    let low = Balance::from_yoctonear(1);
    let mut real = base.clone();
    let hash = real.call("alice.near", "probe.near", "start").args(b"hello").submit();
    real.step(); // the conversion
    real.step(); // start: answer and cb in flight
    real.set_balance("probe.near", low);
    real.settle();
    let out = real.outcome(hash);
    // answer fails on the stake too, so cb gets a failure.
    assert!(matches!(
        out.expect_receipt("answer").failure(),
        Some(Failure::Action(ActionErrorKind::LackBalanceForState { .. }))
    ));
    let want = out.expect_receipt("cb");

    let mut sim = base.clone();
    sim.set_balance("probe.near", low);
    let out = inject(&mut sim, b"hello", [Failed]);
    let got = out.expect_receipt("cb");
    let Some(Failure::Action(ActionErrorKind::LackBalanceForState { account_id, amount })) =
        got.failure()
    else {
        panic!("{out}")
    };
    assert_eq!(got.failure(), want.failure());
    assert_eq!((account_id.as_str(), got.gas_burnt), ("probe.near", want.gas_burnt));
    assert!(amount > low);
    real.findings().assert_none();
    sim.findings().assert_none();
}

/// The hash doesn't cover the results, so a copy of a submitted transaction
/// (the same nonce) must come with the same ones. One that differs, a plain
/// call's copy with results included, panics, and the first keeps its own;
/// an identical copy is queued once, as any.
#[test]
fn a_copy_of_a_submitted_transaction_keeps_its_results() {
    let base = callback_probe_sim();
    let nonce = base.access_keys("alice.near")[0].1.nonce + 1;
    let cb = |sim: &mut Sim, results: Option<Vec<PromiseResult>>| {
        let tx = sim.call("alice.near", "probe.near", "cb").gas(Gas::from_teragas(20)).nonce(nonce);
        match results {
            Some(r) => tx.promise_results(r).submit(),
            None => tx.submit(),
        }
    };
    let copy =
        |sim: &mut Sim, results| panic_message(catch_unwind(AssertUnwindSafe(|| cb(sim, results))));
    let ok = Some(vec![Successful(b"ok".to_vec())]);

    let mut sim = base.clone();
    let hash = cb(&mut sim, ok.clone());
    assert_eq!(cb(&mut sim, ok.clone()), hash);
    let other = copy(&mut sim, Some(vec![Failed]));
    assert_eq!(
        other,
        format!(
            "transaction {hash} was submitted before with other promise results or another predecessor: its hash \
             doesn't cover them, so a copy must have the same"
        )
    );
    assert!(copy(&mut sim, None).contains("submitted before with promise results"));
    sim.settle();
    let out = sim.outcome(hash);
    let s = seen(out.expect_receipt("cb"));
    assert_eq!((s.predecessor.as_str(), s.results.len()), ("probe.near", 1));
    assert!(s.results[0].is_some(), "the first copy's success");

    let mut sim = base.clone();
    let hash = cb(&mut sim, None);
    assert!(copy(&mut sim, ok.clone()).contains("submitted before as a plain call"));
    sim.settle();
    let r = sim.outcome(hash);
    let r = r.expect_receipt("cb");
    assert!(!r.delivered);
    assert_eq!(seen(r).predecessor, "alice.near");
    sim.findings().assert_none();
}
