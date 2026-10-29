//! `Stub`: declared methods, their replies, and the calls an account got.
//! The last tests put a stub behind the near-sdk fungible token as its
//! receiver.

mod common;
mod fuzz_common;
use common::{ft_balance, ft_sim, panic_message};
use near_contract_sim::explore::{Harness, Op, Skip, plans};
use near_contract_sim::nearcore::near_primitives::errors::{FunctionCallError, MethodResolveError};
use near_contract_sim::{
    ActionErrorKind, Balance, Failure, Fault, Gas, Receipts, Reply, Sim, Stub, Tx,
};
use proptest::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::panic::AssertUnwindSafe;

fn sim_with_alice() -> Sim {
    let mut sim = Sim::new();
    sim.create_account("alice.near", Balance::from_near(100));
    sim
}

#[test]
fn each_reply_does_what_it_says() {
    let mut sim = sim_with_alice();
    let stub = Stub::new()
        .on("json", Reply::json(json!({"ok": true})))
        .on("bytes", Reply::bytes("{"))
        .on("panic", Reply::panic("TCB collateral expired").logs(["checking", "EVENT_JSON:{}"]))
        .on("logs", Reply::json("done").logs(["one", "EVENT_JSON:{}"]))
        .on("burn", Reply::json(1).burn_leaving(Gas::from_teragas(2)))
        .on("number", Reply::json(u128::MAX))
        .on("u128", Reply::json(u128::MAX.to_string()));
    sim.deploy_stub("stub.near", stub);
    let call = |sim: &mut Sim, method| {
        sim.call("alice.near", "stub.near", method).gas(Gas::from_teragas(100)).transact()
    };

    assert_eq!(call(&mut sim, "json").json::<Value>(), json!({"ok": true}));
    assert_eq!(call(&mut sim, "bytes").value(), Some(&b"{"[..]));

    let out = call(&mut sim, "panic");
    assert_eq!(out.assert_failure(), Failure::Panic("TCB collateral expired".into()));
    assert_eq!(out.receipt("panic").unwrap().logs, ["checking", "EVENT_JSON:{}"]);
    // A failed receipt's events didn't happen.
    assert!(out.events().is_empty(), "{out}");

    let out = call(&mut sim, "logs");
    assert_eq!(out.json::<String>(), "done");
    assert_eq!(out.receipt("logs").unwrap().logs, ["one", "EVENT_JSON:{}"]);
    assert_eq!(out.events(), [json!({})]);

    // A view's `u128()` reads a near-sdk `U128` (a string) or a number.
    assert_eq!(sim.view("stub.near", "u128", ()).u128(), u128::MAX);
    assert_eq!(sim.view("stub.near", "number", ()).u128(), u128::MAX);

    let out = call(&mut sim, "burn");
    assert_eq!(out.json::<u32>(), 1);
    let headroom = out.receipt("burn").unwrap().headroom();
    assert!(headroom <= Gas::from_teragas(2) && headroom > Gas::from_teragas(1), "{out}");
    // Without a burn, a stub's call costs little: its gas isn't the real
    // contract's.
    let quiet = call(&mut sim, "json");
    assert!(quiet.receipt("json").unwrap().headroom() > Gas::from_teragas(90), "{quiet}");
}

/// `Reply::json(())` answers as a near-sdk method returning `()` does:
/// with no bytes, where `None` is JSON `null`.
#[test]
fn a_void_reply_returns_nothing() {
    let mut sim = sim_with_alice();
    let stub = Stub::new().on("void", Reply::json(())).on("none", Reply::json(None::<u8>));
    sim.deploy_stub("stub.near", stub);
    let void = sim.call("alice.near", "stub.near", "void").transact();
    assert_eq!(void.value(), Some(&b""[..]), "{void}");
    let none = sim.call("alice.near", "stub.near", "none").transact();
    assert_eq!(none.value(), Some(&b"null"[..]), "{none}");
    // A drawn answer in the valid shape is void too.
    let answer = near_contract_sim::explore::input::Answer::Returns(
        near_contract_sim::explore::input::Shape::Valid,
    );
    sim.stub_reply("stub.near", "void", answer.reply(()));
    let void = sim.call("alice.near", "stub.near", "void").transact();
    assert_eq!(void.value(), Some(&b""[..]), "{void}");
}

/// `TxOutcome::u128` reads a call's `U128` (a JSON string) or a JSON
/// number, as `ViewResult::u128` reads a view's, and panics naming the
/// value otherwise.
#[test]
fn a_calls_u128_return() {
    let mut sim = sim_with_alice();
    let stub = Stub::new()
        .on("string", Reply::json("40"))
        .on("number", Reply::json(7))
        .on("word", Reply::json("forty"));
    sim.deploy_stub("stub.near", stub);
    let mut call = |m: &str| sim.call("alice.near", "stub.near", m).transact();
    assert_eq!(call("string").u128(), 40);
    assert_eq!(call("number").u128(), 7);
    let word = call("word");
    let msg = panic_message(std::panic::catch_unwind(AssertUnwindSafe(|| word.u128())));
    assert!(msg.starts_with("\"forty\" isn't a u128"), "{msg}");
}

/// A reply is the stub's storage: setting it on a clone leaves the original
/// as it was.
#[test]
fn stub_reply_in_a_clone_leaves_the_original_alone() {
    let mut sim = sim_with_alice();
    sim.deploy_stub("verifier.near", Stub::new().on("verify_quote", Reply::json(true)));
    let mut expired = sim.clone();
    expired.stub_reply("verifier.near", "verify_quote", Reply::panic("TCB collateral expired"));

    let verify = |sim: &mut Sim| sim.call("alice.near", "verifier.near", "verify_quote").transact();
    assert!(verify(&mut sim).json::<bool>());
    assert_eq!(
        verify(&mut expired).assert_failure(),
        Failure::Panic("TCB collateral expired".into())
    );
    assert!(verify(&mut sim).json::<bool>());
}

#[test]
fn an_undeclared_method_does_not_resolve() {
    let mut sim = sim_with_alice();
    sim.deploy_stub("stub.near", Stub::new().on("a", Reply::json(1)).on("b", Reply::json(2)));
    let out = sim.call("alice.near", "stub.near", "c").transact();
    let unresolved = FunctionCallError::MethodResolveError(MethodResolveError::MethodNotFound);
    assert_eq!(
        out.assert_failure(),
        Failure::Action(ActionErrorKind::FunctionCallError(unresolved))
    );

    let set = std::panic::catch_unwind(AssertUnwindSafe(|| {
        sim.stub_reply("stub.near", "c", Reply::json(3));
    }));
    assert_eq!(panic_message(set), "the stub at stub.near declares no `c`; it declares: a, b");
    let set = std::panic::catch_unwind(AssertUnwindSafe(|| {
        sim.stub_reply("alice.near", "a", Reply::json(3));
    }));
    assert_eq!(panic_message(set), "alice.near is no stub: deploy one with Sim::deploy_stub");
}

/// Declaring a method again replaces its reply; any name works.
#[test]
fn a_stub_declares_each_method_once() {
    let mut sim = sim_with_alice();
    let stub = Stub::new()
        .on("m", Reply::json(1))
        .on("say \"hi\"\\", Reply::json(2))
        .on("m", Reply::json(3));
    sim.deploy_stub("stub.near", stub);
    assert_eq!(sim.call("alice.near", "stub.near", "m").transact().json::<u32>(), 3);
    assert_eq!(sim.call("alice.near", "stub.near", "say \"hi\"\\").transact().json::<u32>(), 2);
    // Deploying over the stub replaces it.
    sim.deploy_stub("stub.near", Stub::new().on("n", Reply::json(4)));
    sim.call("alice.near", "stub.near", "m").transact().assert_failure();
    assert_eq!(sim.call("alice.near", "stub.near", "n").transact().json::<u32>(), 4);
}

fn send_tx(sim: &mut Sim, amount: u128) -> Tx<'_> {
    sim.call("alice.near", "ft.near", "ft_transfer_call")
        .args_json(json!({"receiver_id": "r.near", "amount": amount.to_string(), "msg": ""}))
        .deposit(Balance::from_yoctonear(1))
        .gas(Gas::from_teragas(100))
}

/// The near-sdk fungible token, with a stub as `r.near`.
fn ft_with_stub(reply: Reply) -> Sim {
    let mut sim = ft_sim();
    sim.deploy_stub("r.near", Stub::new().on("ft_on_transfer", reply));
    sim
}

/// `calls_to` lists every call an account got, a failed one too, with who
/// sent it, what it attached and its args; on any account, not only stubs.
#[test]
fn calls_to_lists_failed_calls_too() {
    let mut sim = ft_with_stub(Reply::panic("no thanks"));
    send_tx(&mut sim, 100).transact().assert_success();

    let calls = sim.calls_to("r.near");
    let [call] = &calls[..] else { panic!("{calls:#?}") };
    assert_eq!((call.method_name.as_str(), call.succeeded), ("ft_on_transfer", false));
    assert_eq!((call.predecessor_id.as_str(), call.signer_id.as_str()), ("ft.near", "alice.near"));
    assert_eq!(call.deposit, Balance::ZERO);
    assert!(call.gas > Gas::from_teragas(10), "{call:?}");
    let args: Value = call.args_json().unwrap();
    assert_eq!(args, json!({"sender_id": "alice.near", "amount": "100", "msg": ""}));
    assert_eq!(ft_balance(&sim, "r.near"), "0");

    // ft.near isn't a stub: its calls are there too, deposits included.
    let to_ft = sim.calls_to("ft.near");
    let transfer = to_ft.iter().find(|c| c.method_name == "ft_transfer_call").unwrap();
    assert_eq!(
        (transfer.predecessor_id.as_str(), transfer.deposit),
        ("alice.near", Balance::from_yoctonear(1))
    );
    assert_eq!(transfer.gas, Gas::from_teragas(100));
    assert!(transfer.succeeded);
    let resolve = to_ft.iter().find(|c| c.method_name == "ft_resolve_transfer").unwrap();
    assert_eq!(resolve.predecessor_id.as_str(), "ft.near");
    assert!(resolve.block_height > transfer.block_height);
    // A transaction to its own signer runs a local receipt, without a body.
    let init = &to_ft[0];
    assert_eq!(
        (init.method_name.as_str(), init.predecessor_id.as_str()),
        ("new_default_meta", "ft.near")
    );
    assert!(init.succeeded);
}

/// The token keeps what the receiver says it used, at most what was sent,
/// and refunds everything when the receiver fails. A failed receipt keeps
/// the logs it made before the limit stopped it.
#[test]
fn the_token_settles_each_reply() {
    let base = ft_with_stub(Reply::json("0"));
    let lines = |n: usize, len: usize| vec!["x".repeat(len); n];
    let cases = [
        (Reply::json("0"), "100", 0),
        (Reply::json("40"), "60", 0),
        (Reply::json("100"), "0", 0),
        (Reply::json("150"), "0", 0),
        (Reply::json(u128::MAX.to_string()), "0", 0),
        (Reply::bytes("{"), "0", 0),
        (Reply::bytes(r#"["40"]"#), "0", 0),
        (Reply::panic("no thanks"), "0", 0),
        (Reply::json("0").logs(lines(100, 163)), "100", 100),
        (Reply::json("0").logs(lines(101, 1)), "0", 100),
    ];
    for (reply, kept, logs) in cases {
        let mut sim = base.clone();
        sim.stub_reply("r.near", "ft_on_transfer", reply.clone());
        let out = send_tx(&mut sim, 100).transact();
        assert_eq!(ft_balance(&sim, "r.near"), kept, "{reply:?}\n{out}");
        assert_eq!(out.receipt("ft_on_transfer").unwrap().logs.len(), logs, "{reply:?}");
    }
}

/// A reply bound to a transaction (`Tx::stub_reply`) answers only that
/// transaction's calls, even when another's call to the same method runs
/// in the same block; every other caller gets the stub-wide reply, which
/// `Sim::stub_reply` changes without touching a bound one.
#[test]
fn a_bound_reply_answers_only_its_own_transactions_calls() {
    let mut sim = ft_with_stub(Reply::json("0"));
    let first = send_tx(&mut sim, 100).stub_reply("r.near", "ft_on_transfer", Reply::json("40"));
    let first = first.submit();
    let second = send_tx(&mut sim, 101).stub_reply("r.near", "ft_on_transfer", Reply::json("90"));
    let second = second.submit();
    // Unbound: the stub-wide reply, changed after the others were bound.
    let third = send_tx(&mut sim, 102).submit();
    sim.stub_reply("r.near", "ft_on_transfer", Reply::json("2"));
    sim.settle();

    let calls = sim.calls_to("r.near");
    assert_eq!(calls.len(), 3, "{calls:#?}");
    assert!(calls.iter().all(|c| c.block_height == calls[0].block_height), "one block");
    // Each settles with the token by its own reply, which names what it
    // didn't use: 40 of 100, 90 of 101 and 2 of 102 back.
    let used: Vec<u128> = [first, second, third]
        .map(|tx| {
            let out = sim.outcome(tx);
            let used = out.receipt("ft_resolve_transfer").unwrap().value().unwrap().to_vec();
            serde_json::from_slice::<String>(&used).unwrap().parse().unwrap()
        })
        .into();
    assert_eq!(used, [60, 11, 100]);
    assert_eq!(ft_balance(&sim, "r.near"), "171");
    // The keys are gone once the calls ran: nothing but the stub's own.
    let keys: Vec<String> = (sim.view_state("r.near", b"reply/").into_iter())
        .map(|(k, _)| String::from_utf8_lossy(&k).into_owned())
        .collect();
    assert_eq!(keys, ["reply/0"]);
}

/// Binding one method several times queues the replies: the
/// transaction's calls to it take them in the order they reach the stub,
/// and the last answers the calls after that.
#[test]
fn replies_bound_to_one_method_answer_its_calls_in_order() {
    use fuzz_common::{Op as Fz, TGAS, encode, fuzz_wasm};
    let mut sim = near_contract_sim::Fixture::new()
        .account("alice.near", Balance::from_near(10))
        .stub("s.near", Stub::new().on("run", Reply::json(0)))
        .contract("caller.near", fuzz_wasm())
        .build();
    let call =
        Fz::Call { to: "s.near".into(), args: encode(&[]), deposit: 0, gas: 5 * TGAS, weight: 0 };
    let out = sim
        .call("alice.near", "caller.near", "run")
        .args(encode(&[call.clone(), call.clone(), call]))
        .stub_reply("s.near", "run", Reply::json(1))
        .stub_reply("s.near", "run", Reply::json(2))
        .transact();
    let answers: Vec<String> = (out.receipts.iter())
        .filter(|r| r.receiver_id == "s.near")
        .map(|r| String::from_utf8_lossy(r.value().unwrap()).into_owned())
        .collect();
    assert_eq!(answers, ["1", "2", "2"], "{out}");
}

/// A held call keeps its bound reply until it runs, blocks later.
#[test]
fn a_held_call_keeps_its_bound_reply() {
    let mut sim = ft_with_stub(Reply::json("0"));
    let held = send_tx(&mut sim, 100)
        .stub_reply("r.near", "ft_on_transfer", Reply::json("30"))
        .inject(Receipts::method("ft_on_transfer"), Fault::Hold(3))
        .submit();
    let plain = send_tx(&mut sim, 101).submit();
    sim.settle();
    let calls = sim.calls_to("r.near");
    assert!(calls[1].block_height >= calls[0].block_height + 3, "{calls:#?}");
    assert_eq!(ft_balance(&sim, "r.near"), (70 + 101).to_string());
    assert!(sim.outcome(held).is_success() && sim.outcome(plain).is_success());
}

/// A bound reply that panics fails only its own call; and binding a
/// method the stub doesn't declare panics at once.
#[test]
fn a_bound_reply_can_panic_and_must_name_a_declared_method() {
    let mut sim = ft_with_stub(Reply::json("0"));
    let bad = send_tx(&mut sim, 100).stub_reply("r.near", "ft_on_transfer", Reply::panic("no"));
    let bad = bad.submit();
    let good = send_tx(&mut sim, 101).submit();
    sim.settle();
    let failed: Vec<bool> = [bad, good]
        .map(|tx| !sim.outcome(tx).receipt("ft_on_transfer").unwrap().is_success())
        .into();
    assert_eq!(failed, [true, false]);
    assert_eq!(ft_balance(&sim, "r.near"), "101");

    let msg = panic_message(std::panic::catch_unwind(AssertUnwindSafe(|| {
        let _ = send_tx(&mut sim, 1).stub_reply("r.near", "nope", Reply::json("0"));
    })));
    assert!(msg.contains("declares no `nope`"), "{msg}");

    // A delegate is relayed in another transaction: binding on it panics
    // rather than silently dropping the reply.
    let msg = panic_message(std::panic::catch_unwind(AssertUnwindSafe(|| {
        let _ = send_tx(&mut sim, 1)
            .stub_reply("r.near", "ft_on_transfer", Reply::json("0"))
            .relay("alice.near");
    })));
    assert!(msg.contains("a delegate carries no stub replies"), "{msg}");
}

/// A reply longer than the template's one memory page (nearcore gives the
/// contract its own memory).
#[test]
fn a_long_reply_comes_back_whole() {
    let mut sim = sim_with_alice();
    let value = vec![b'7'; 1 << 20];
    sim.deploy_stub("r.near", Stub::new().on("ft_on_transfer", Reply::bytes(value.clone())));
    let out =
        sim.call("alice.near", "r.near", "ft_on_transfer").gas(Gas::from_teragas(300)).transact();
    assert_eq!(out.value(), Some(&value[..]), "{out}");
}

fn supply(sim: &Sim) -> Result<(), String> {
    let sum: u128 = ["alice.near", "r.near"]
        .iter()
        .map(|account| ft_balance(sim, account).parse::<u128>().unwrap())
        .sum();
    if sum == 1000 { Ok(()) } else { Err(format!("token supply is {sum}, expected 1000")) }
}

/// One transfer to r.near, after setting what its stub replies.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Send {
    /// The reply's bytes, unless it panics.
    value: String,
    panic: bool,
    /// How many log lines, and how long each.
    logs: (usize, usize),
    /// Burn gas down to 1 Tgas left.
    burn: bool,
    amount: u64,
}

impl Op for Send {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        let mut reply =
            if self.panic { Reply::panic("no thanks") } else { Reply::bytes(self.value.clone()) };
        reply = reply.logs(vec!["x".repeat(self.logs.1); self.logs.0]);
        if self.burn {
            reply = reply.burn_leaving(Gas::from_teragas(1));
        }
        sim.stub_reply("r.near", "ft_on_transfer", reply);
        send_tx(sim, self.amount.into()).submit();
        Ok(())
    }
}

/// No reply the receiver gives, value, panic, logs or gas burnt, makes the
/// token's `ft_resolve_transfer` fail or changes the supply, under
/// overlapping transfers and the default tolerable faults.
#[test]
fn ft_resolve_transfer_is_total() {
    let max = format!("\"{}\"", u128::MAX);
    let values = [r#""0""#, r#""100""#, r#""-1""#, "null", "{", max.as_str()];
    let values: Vec<String> = values.map(String::from).to_vec();
    let sends = (
        prop::sample::select(values),
        any::<bool>(),
        prop::sample::select(vec![(0, 0), (100, 163), (101, 1)]),
        any::<bool>(),
        1u64..=100,
    )
        .prop_map(|(value, panic, logs, burn, amount)| Send {
            value,
            panic,
            logs,
            burn,
            amount,
        });
    let report = Harness::new(ft_with_stub(Reply::json("0")))
        .at_rest("no callback fails", |sim: &Sim| {
            let failed: Vec<_> = sim
                .calls_to("ft.near")
                .into_iter()
                .filter(|c| c.method_name == "ft_resolve_transfer" && !c.succeeded)
                .collect();
            if failed.is_empty() { Ok(()) } else { Err(format!("{failed:?}")) }
        })
        .at_rest("supply", supply)
        // The repeat draw moves seed 0x5eed's stream onto a starved
        // resolver; off, the stream is as pinned.
        .search(plans(sends).len(1..=8).repeats(false))
        .seed(0x5eed)
        .cases(8)
        .run();
    report.assert_clean();
    report.assert_reached("ft.near::ft_resolve_transfer");
}

/// A reply in the wrong JSON shape refunds the whole transfer; one in the
/// right shape (the unused amount) refunds that much and the receiver keeps
/// the rest. `ft_transfer_call` returns what was used. Both branches of the
/// token's resolve, on real receipts.
#[test]
fn a_reply_in_the_wrong_shape_refunds_everything() {
    let send = |reply: Reply, amount: u128| {
        let mut sim = ft_with_stub(reply);
        let out = send_tx(&mut sim, amount).transact();
        out.assert_success();
        (out.json::<String>(), ft_balance(&sim, "r.near"))
    };
    assert_eq!(send(Reply::bytes(r#"["40"]"#), 100), ("0".into(), "0".into()));
    assert_eq!(send(Reply::json("40"), 40), ("0".into(), "0".into()));
    assert_eq!(send(Reply::json("40"), 100), ("60".into(), "60".into()));
}

/// A stub answers views with its stub-wide reply, as it answers calls:
/// a stubbed token serves `ft_balance_of`, and `Sim::stub_reply` changes what a
/// view sees. A panic reply fails the view; a `burn_leaving` one can't
/// serve one (a view has no prepaid gas).
#[test]
fn a_stub_answers_views() {
    let mut sim = sim_with_alice();
    let token = Stub::new()
        .on("ft_balance_of", Reply::json("500").logs(["balance read"]))
        .on("ft_total_supply", Reply::panic("paused"))
        .on("slow", Reply::json(1).burn_leaving(Gas::from_teragas(2)));
    sim.deploy_stub("token.near", token);
    let balance = sim.view("token.near", "ft_balance_of", json!({ "account_id": "alice.near" }));
    assert_eq!(balance.json::<String>(), "500");
    assert_eq!(balance.logs(), ["balance read"]);
    sim.stub_reply("token.near", "ft_balance_of", Reply::json("7"));
    let balance = sim.view("token.near", "ft_balance_of", json!({ "account_id": "alice.near" }));
    assert_eq!(balance.json::<String>(), "7");
    let paused = sim.view("token.near", "ft_total_supply", ()).result.unwrap_err();
    assert!(matches!(&paused, Failure::Panic(m) if m == "paused"), "{paused:?}");
    let slow = sim.view("token.near", "slow", ()).result.unwrap_err();
    assert!(slow.to_string().contains("prepaid_gas"), "{slow}");
    // Calls are unchanged.
    let call = sim.call("alice.near", "token.near", "ft_balance_of").transact();
    assert_eq!(call.json::<String>(), "7");
}

/// A reply that calls first: the call goes out with its args, deposit and
/// the stub's unused gas, and the caller gets the answer. A panicking
/// answer fails the receipt, and drops the call. A view can't call.
#[test]
fn a_reply_can_call_before_it_answers() {
    let mut sim = sim_with_alice();
    sim.deploy_stub("bob.near", Stub::new().on("ping", Reply::json("pong")));
    let reply = Reply::call("bob.near", "ping", json!({ "n": 1 }))
        .deposit(Balance::from_yoctonear(1))
        .then(Reply::json("answered").logs(["called first"]));
    sim.deploy_stub("token.near", Stub::new().on("ask", reply).on("refuse", Reply::panic("no")));
    let out = sim.call("alice.near", "token.near", "ask").transact();
    assert_eq!(out.json::<String>(), "answered", "{out}");
    let pings = sim.calls_to("bob.near");
    assert_eq!(pings.len(), 1, "{out}");
    assert_eq!(pings[0].predecessor_id.as_str(), "token.near");
    assert_eq!(pings[0].args_json::<serde_json::Value>().unwrap(), json!({ "n": 1 }));
    assert_eq!(pings[0].deposit, Balance::from_yoctonear(1));
    assert!(pings[0].succeeded && pings[0].gas > Gas::from_teragas(100), "{out}");

    let failing = Reply::call("bob.near", "ping", ()).then(Reply::panic("refused"));
    let out = sim
        .call("alice.near", "token.near", "refuse")
        .stub_reply("token.near", "refuse", failing)
        .transact();
    assert!(!out.is_success());
    assert_eq!(sim.calls_to("bob.near").len(), 1, "the panic dropped the call");

    let view = sim.view("token.near", "ask", ()).result.unwrap_err();
    assert!(view.to_string().contains("promise_batch_create"), "{view}");
}

/// `burn(gas)` burns a fixed amount, whatever is attached:
/// at least 5 Tgas used, and attached less it runs out.
///
/// Verification boundary: the stub template's behavior, against actual
/// runs of the pinned runtime; no scheduling change.
#[test]
fn burn_uses_a_fixed_amount() {
    let mut sim = sim_with_alice();
    let five = Gas::from_teragas(5);
    sim.deploy_stub("w.near", Stub::new().on("work", Reply::json(1).burn(five)));
    let call = |sim: &mut Sim, gas| sim.call("alice.near", "w.near", "work").gas(gas).transact();
    let out = call(&mut sim, Gas::from_teragas(100));
    out.assert_success();
    let r = out.receipt("work").unwrap();
    assert!(r.gas_burnt >= five, "{out}");
    assert!(r.headroom() > Gas::from_teragas(90), "it burns 5, not down to a floor: {out}");
    assert_eq!(call(&mut sim, Gas::from_teragas(4)).assert_failure(), Failure::OutOfGas);
}

/// `fail_below(X)` fails at X - 1 gas, naming what was
/// attached, and succeeds at X; through a relay that forwards all it has
/// left, a caller's gas at or below X fails.
///
/// Verification boundary: the stub template's behavior, against actual
/// runs of the pinned runtime.
#[test]
fn fail_below_fails_under_its_threshold() {
    use near_contract_sim::explore::{Call, Plan};
    let x = Gas::from_teragas(20);
    let mut sim = sim_with_alice();
    sim.deploy_stub("w.near", Stub::new().on("work", Reply::json(1).fail_below(x)));
    let relay = Reply::call("w.near", "work", json!({})).then(Reply::json(1));
    sim.deploy_stub("relay.near", Stub::new().on("go", relay));
    let call = |sim: &mut Sim, gas| sim.call("alice.near", "w.near", "work").gas(gas).transact();
    let below = Gas::from_gas(x.as_gas() - 1);
    let want = format!("stub: attached {} gas below {}", below.as_gas(), x.as_gas());
    assert_eq!(call(&mut sim, below).assert_failure(), Failure::Panic(want));
    call(&mut sim, x).assert_success();

    let harness = Harness::<Call>::new(sim).must_not_fail("work");
    let plan = Plan::new().op(Call::new("alice.near", "relay.near", "go"));
    // The caller's gas, below and above what the relay needs to forward X.
    let at = |g: u64| harness.run(&plan.clone().gas(Gas::from_teragas(g)));
    for g in [5, 10, 20] {
        let run = at(g);
        assert!(!run.findings().is_empty(), "{g} Tgas: {run}");
    }
    at(100).assert_clean();
}

/// `Shape::Limit` answers with the largest reply the host
/// returns (`max_length_returned_data`, from the sim's config); one byte
/// more fails with the host's own error. The stub pads as it runs: a 4 MiB
/// reply read from storage would pass the per-receipt storage proof limit
/// first.
///
/// Verification boundary: the stub's reply against the pinned runtime's
/// limits; no scheduling change.
#[test]
fn a_reply_at_the_limit_returns_and_one_byte_more_fails() {
    use near_contract_sim::explore::input::{Answer, Shape};
    let mut sim = sim_with_alice();
    let limits = &sim.config.runtime_config().wasm_config.limit_config;
    let limit = limits.max_length_returned_data as usize;
    let at = Answer::Returns(Shape::Limit).reply(json!("x"));
    let over = Answer::Returns(Shape::Bytes(limit + 1)).reply(json!("x"));
    sim.deploy_stub("big.near", Stub::new().on("at", at).on("over", over));
    let call = |sim: &mut Sim, m| {
        sim.call("alice.near", "big.near", m).gas(Gas::from_teragas(300)).transact()
    };
    let out = call(&mut sim, "at");
    out.assert_success();
    let value = out.value().expect("a value");
    assert_eq!(value.len(), limit);
    assert_eq!(serde_json::from_slice::<String>(value).unwrap(), "x");
    assert_eq!(Shape::Limit.json(json!("x")).len(), limit);
    let failure = call(&mut sim, "over").assert_failure();
    let want = format!("length of a returned value {} exceeds the limit {limit}", limit + 1);
    assert!(format!("{failure:?}").contains(&want), "{failure:?}");
}

/// A bound reply sits over the stub-wide one: the logs, call, gas and
/// padding it doesn't set are the stub-wide reply's when it is bound;
/// those it sets, and its value or panic, are its own.
#[test]
fn a_bound_reply_keeps_what_it_leaves_unset_from_the_stub_wide_one() {
    let mut sim = sim_with_alice();
    let wide = Reply::json(0).logs(["wide"]).burn(Gas::from_teragas(5)).pad_to(8);
    sim.deploy_stub("s.near", Stub::new().on("m", wide));
    let call = |sim: &mut Sim, reply: Reply| {
        let tx = sim.call("alice.near", "s.near", "m").gas(Gas::from_teragas(30));
        tx.stub_reply("s.near", "m", reply).transact()
    };
    let plain = sim.call("alice.near", "s.near", "m").gas(Gas::from_teragas(30)).transact();
    let merged = call(&mut sim, Reply::json(7));
    assert_eq!(merged.value(), Some(&b"7       "[..]), "{merged}");
    assert_eq!(merged.receipts[0].logs, ["wide"]);
    let burnt = |out: &near_contract_sim::TxOutcome| out.receipts[0].gas_burnt.as_gas();
    assert!(burnt(&merged) >= Gas::from_teragas(5).as_gas(), "{merged}");
    assert!(burnt(&merged).abs_diff(burnt(&plain)) < Gas::from_teragas(1).as_gas(), "{merged}");
    let own = call(&mut sim, Reply::panic("no").logs(["own"]));
    assert_eq!(own.receipts[0].logs, ["own"]);
    assert_eq!(own.failure(), Some(Failure::Panic("no".into())), "{own}");
}
