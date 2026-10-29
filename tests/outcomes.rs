//! `TxOutcome` / `ViewResult` methods, the asserting finishers and the
//! rendering they append.

mod common;
use common::*;
use near_contract_sim::nearcore::near_primitives::errors::{
    CompilationError, FunctionCallError, MethodResolveError,
};
use near_contract_sim::{
    AccessKey, ActionErrorKind, Balance, Failure, FinalExecutionStatus, Gas, KeyType, SecretKey,
    Sim,
};
use serde_json::{Value, json};
use std::panic::AssertUnwindSafe;

/// `view` sends its args as JSON, and args that serialize to `null` as no
/// input; `view_raw` sends them as they are. A view fails as a call does.
#[test]
fn views() {
    let mut sim = sim_with_users();
    sim.deploy("env.near", env_wasm());
    sim.deploy("status.near", status_wasm());
    let echo = |args| sim.view("env.near", "echo", args).result.unwrap();
    assert_eq!(echo(json!({"a": 1})), br#"{"a":1}"#);
    assert_eq!(echo(json!(null)), b"");
    assert_eq!(sim.view("env.near", "echo", ()).result.unwrap(), b"");
    let raw = sim.view_raw("env.near", "echo", b"\"hi\"");
    assert_eq!(raw.json::<String>(), "hi");
    assert_eq!(sim.view_raw("env.near", "echo", b"null").result.unwrap(), b"null");
    let err = |id, m, args| sim.view(id, m, args).result.unwrap_err();
    let unresolved = FunctionCallError::MethodResolveError(MethodResolveError::MethodNotFound);
    let unresolved = Failure::Action(ActionErrorKind::FunctionCallError(unresolved));
    assert_eq!(err("env.near", "no_such_method", json!(null)), unresolved);
    let missing = Failure::AccountDoesNotExist("nobody.near".parse().unwrap());
    assert_eq!(err("nobody.near", "m", json!(null)), missing);
    let no_code = CompilationError::CodeDoesNotExist { account_id: "alice.near".parse().unwrap() };
    let no_code = ActionErrorKind::FunctionCallError(FunctionCallError::CompilationError(no_code));
    assert_eq!(err("alice.near", "m", json!(null)), Failure::Action(no_code));
    let set = err("status.near", "set_status", json!({"message": "hi"}));
    assert_eq!(set, Failure::Host("signer_account_id is not allowed in view calls".into()));
    let get = err("status.near", "get_status", json!({}));
    assert!(matches!(&get, Failure::Panic(m) if m.contains("deserialize")), "{get}");
}

#[test]
fn view_failures_name_the_account_method_and_block() {
    let mut sim = sim_with_users();
    sim.deploy("status.near", status_wasm());
    let view = sim.view("status.near", "typo_get_status", ());
    assert_eq!(view.receiver_id.as_str(), "status.near");
    assert_eq!(view.method_name, "typo_get_status");
    assert_eq!(view.block_height, sim.height());
    assert!(matches!(view.assert_failure(), Failure::Action(_)));
    let message = panic_message(std::panic::catch_unwind(|| view.json::<serde_json::Value>()));
    assert!(
        message.contains(&format!("view status.near.typo_get_status at #{} failed:", sim.height())),
        "{message}"
    );
    assert!(message.contains("MethodNotFound"), "{message}");
    let ok = sim.view("status.near", "get_status", json!({ "account_id": "alice.near" }));
    let message = panic_message(std::panic::catch_unwind(|| ok.assert_failure()));
    assert!(message.contains("get_status at #") && message.contains("not a failure"), "{message}");

    let absent = sim.view("nobody.near", "get_status", ());
    assert!(matches!(absent.result, Err(Failure::AccountDoesNotExist(_))));
    let message = panic_message(std::panic::catch_unwind(|| absent.json::<serde_json::Value>()));
    assert!(message.contains("view nobody.near.get_status at #"), "{message}");
}

#[test]
fn outcome_helpers_on_failure_and_success() {
    let mut sim = sim_with_users();
    sim.deploy("fail.near", failure_probe_wasm());
    let before = sim.balance("alice.near");
    let out = sim.call("alice.near", "fail.near", "write_then_panic").transact();
    assert!(out.is_failure() && !out.is_success() && !out.is_pending());
    assert!(out.value().is_none());
    assert_eq!(out.failure(), Some(Failure::Panic("boom".into())));
    assert_eq!(out.assert_failure(), Failure::Panic("boom".into()));
    let msg = panic_message(std::panic::catch_unwind(|| out.json::<String>()));
    let first = "first failure: #4 fail.near::write_then_panic: panicked: boom";
    assert_eq!(msg, format!("expected success; {first}\n\n{out}"));
    let failed: Vec<_> = out.receipts.iter().filter(|r| r.failure().is_some()).collect();
    assert_eq!(failed.len(), 1);
    assert!(!failed[0].is_success());
    assert_eq!(failed[0].failure(), out.failure());
    assert_eq!(failed[0].method_names, ["write_then_panic"]);
    assert_eq!(failed[0].predecessor_id.as_str(), "alice.near");
    assert!(out.receipt("write_then_panic").is_some());
    assert!(out.receipt("write").is_none());

    // Gas / token sums, computed independently from the parts.
    let conv = out.transaction.as_ref().unwrap();
    assert!(conv.is_success() && conv.gas_burnt > Gas::ZERO);
    assert!(conv.method_names.is_empty() && conv.attached_gas == Gas::ZERO, "{conv:?}");
    assert_eq!(conv.predecessor_id.as_str(), "alice.near");
    let receipts: u64 = out.receipts.iter().map(|r| r.gas_burnt.as_gas()).sum();
    assert_eq!(out.gas_burnt().as_gas(), conv.gas_burnt.as_gas() + receipts);
    let tokens: u128 = out.receipts.iter().map(|r| r.tokens_burnt.as_yoctonear()).sum::<u128>()
        + conv.tokens_burnt.as_yoctonear();
    assert_eq!(out.tokens_burnt().as_yoctonear(), tokens);
    assert_eq!(before.as_yoctonear() - sim.balance("alice.near").as_yoctonear(), tokens);
    // Only non-zero gas profile entries are reported.
    let fc = out.receipt("write_then_panic").unwrap();
    assert!(!fc.gas_profile.is_empty() && fc.gas_profile.values().all(|g| *g > 0));
    assert!(conv.gas_profile.is_empty());

    // Logs and the value on success.
    let ok = sim.call("alice.near", "fail.near", "send_to_missing").transact();
    assert!(ok.is_success() && !ok.is_failure());
    assert_eq!(ok.failure(), None);
    assert_eq!(ok.logs(), ["cb-failed"]);
    assert_eq!(ok.value(), Some(&b"\"cb-failed\""[..]));
    assert_eq!(ok.json::<String>(), "cb-failed");
}

#[test]
#[should_panic(expected = "unknown transaction")]
fn outcome_of_a_receipt_panics() {
    let mut sim = sim_with_users();
    sim.deploy("fail.near", failure_probe_wasm());
    let out = sim.call("alice.near", "fail.near", "hello").transact();
    let _ = sim.outcome(out.receipts[0].receipt_id);
}

/// `gas_burnt` is the exec fees the sender prepaid plus what the receipt
/// burnt running, out of its attached gas. The numbers themselves are the
/// node's, unchanged.
#[test]
fn gas_burnt_splits_into_prepaid_exec_fees_and_the_run() {
    use near_contract_sim::nearcore::near_parameters::ActionCosts;
    use near_contract_sim::{ReceiptOutcome, TxOutcome};
    let mut sim = sim_with_users();
    sim.deploy("status.near", status_wasm());
    let exec = |c| sim.config.runtime_config().fees.fee(c).exec_fee().gas.as_gas();
    let (receipt, call, byte, transfer) = (
        exec(ActionCosts::new_action_receipt),
        exec(ActionCosts::function_call_base),
        exec(ActionCosts::function_call_byte),
        exec(ActionCosts::transfer),
    );
    let call_fees = |method: &str, args: &str| call + byte * (method.len() + args.len()) as u64;
    let args = json!({"message": "hi"}).to_string();
    let first = |o: &TxOutcome| -> ReceiptOutcome {
        o.receipts.iter().find(|r| !r.is_refund()).unwrap().clone()
    };
    let ran = |r: &ReceiptOutcome| r.gas_burnt.as_gas() - r.prepaid_exec_fees.as_gas();

    let out = sim
        .call("alice.near", "status.near", "set_status")
        .args(args.clone().into_bytes())
        .transact();
    let r = first(&out);
    assert_eq!(r.prepaid_exec_fees.as_gas(), receipt + call_fees("set_status", &args));
    assert!(ran(&r) > 0 && ran(&r) < r.attached_gas.as_gas());
    let refund = out.receipts.iter().find(|r| r.is_refund()).unwrap();
    assert_eq!(refund.prepaid_exec_fees.as_gas(), receipt + transfer);
    assert_eq!(refund.gas_burnt, refund.prepaid_exec_fees);
    assert_eq!(out.transaction.as_ref().unwrap().prepaid_exec_fees, Gas::ZERO);

    // Out of gas: the fees plus exactly the attached gas.
    let out = sim
        .call("alice.near", "status.near", "set_status")
        .args(args.clone().into_bytes())
        .gas(Gas::from_gigagas(200))
        .transact();
    let r = first(&out);
    assert_eq!(r.failure(), Some(Failure::OutOfGas));
    assert_eq!(r.prepaid_exec_fees.as_gas(), receipt + call_fees("set_status", &args));
    assert_eq!(ran(&r), Gas::from_gigagas(200).as_gas());

    // A transfer burns only its fees.
    let out = sim.tx("alice.near", "bob.near").transfer(near(1)).transact();
    let r = first(&out);
    assert_eq!(r.prepaid_exec_fees.as_gas(), receipt + transfer);
    assert_eq!(r.gas_burnt, r.prepaid_exec_fees);

    // The runtime stops at the failing action: the transfer after it is
    // never charged.
    let out = sim
        .tx("alice.near", "status.near")
        .transfer(near(1))
        .call("no_such_method")
        .args(b"{}".to_vec())
        .gas(Gas::from_teragas(10))
        .transfer(near(1))
        .transact();
    let r = first(&out);
    assert!(err(&out).contains("index: Some(1)"), "{}", err(&out));
    let charged = receipt + transfer + call_fees("no_such_method", "{}");
    assert_eq!(r.prepaid_exec_fees.as_gas(), charged);
    assert!(ran(&r) < r.attached_gas.as_gas());
}

/// `headroom()` is the attached gas a receipt had left: less what it burnt
/// running, and less what it prepaid the receipts it created (their attached
/// gas and exec fees). A failed receipt, a transfer and the conversion have
/// none.
#[test]
fn headroom_is_the_attached_gas_left() {
    let mut sim = sim_with_users();
    sim.deploy("fail.near", failure_probe_wasm());
    let out = sim.call("alice.near", "fail.near", "send_to_missing").transact();
    let r = out.receipt("send_to_missing").unwrap();
    let prepaid: u64 = out
        .receipts
        .iter()
        .filter(|c| r.receipt_ids.contains(&c.receipt_id) && !c.is_refund())
        .map(|c| c.attached_gas.as_gas() + c.prepaid_exec_fees.as_gas())
        .sum();
    let ran = r.gas_burnt.as_gas() - r.prepaid_exec_fees.as_gas();
    assert_eq!(r.headroom().as_gas(), r.attached_gas.as_gas() - ran - prepaid, "{out}");
    let ping = out.receipt("ping").unwrap();
    assert!(ping.failure().is_some() && ping.attached_gas > Gas::ZERO, "{out}");
    assert_eq!(ping.headroom(), Gas::ZERO);
    let refund = out.receipts.iter().find(|c| c.is_refund()).unwrap();
    assert_eq!(refund.headroom(), Gas::ZERO);
    assert_eq!(out.transaction.as_ref().unwrap().headroom(), Gas::ZERO);
}

/// `assert_headroom` returns the headroom, and tells a missing receipt, a
/// failed one, one that left too little and one that gave its unused gas to
/// its calls apart.
#[test]
fn assert_headroom_says_why() {
    let mut sim = sim_with_users();
    sim.deploy("fail.near", failure_probe_wasm());
    let out = sim.call("alice.near", "fail.near", "send_to_missing").transact();
    let left = out.expect_receipt("send_to_missing").headroom();
    assert!(left > Gas::ZERO, "{out}");
    assert_eq!(out.assert_headroom("send_to_missing", left), left);
    let panics = |f: &dyn Fn()| panic_message(std::panic::catch_unwind(AssertUnwindSafe(f)));
    let trace = format!("\n\n{out}");
    assert_eq!(panics(&|| _ = out.expect_receipt("nope")), format!("no receipt ran nope{trace}"));
    let msg = panics(&|| _ = out.assert_headroom("nope", Gas::ZERO));
    assert_eq!(msg, format!("no receipt ran nope{trace}"));
    let tgas = |g: Gas| format!("{:.2} Tgas", g.as_gas() as f64 / 1e12);
    let more = left.checked_add(Gas::from_teragas(1)).unwrap();
    let msg = panics(&|| _ = out.assert_headroom("send_to_missing", more));
    let below = format!("expected send_to_missing to leave {}, it left {}", tgas(more), tgas(left));
    assert_eq!(msg, format!("{below}{trace}"));
    let msg = panics(&|| _ = out.assert_headroom("ping", Gas::ZERO));
    let failed = "expected ping to leave 0.00 Tgas, it failed: account missing.near does not exist";
    assert_eq!(msg, format!("{failed}{trace}"));

    sim.deploy("factorial.near", xcc_wasm());
    let out =
        sim.call("alice.near", "factorial.near", "factorial").args_json(json!({"n": 2})).transact();
    let msg = panics(&|| _ = out.assert_headroom("factorial", Gas::ZERO));
    let gave = "expected factorial to leave 0.00 Tgas, it kept no headroom and its unused gas \
                went to its calls, as weighted forwarding does: assert on theirs; if their gas \
                is fixed, it is at its own limit";
    assert_eq!(msg, format!("{gave}\n\n{out}"));

    // A call given exactly the gas it burns running keeps none either, but
    // made no calls: 0 meets a minimum of 0.
    let leaf = |gas| {
        let mut sim = sim_with_users();
        sim.deploy("env.near", env_wasm());
        sim.call("alice.near", "env.near", "echo").gas(gas).transact()
    };
    let r = leaf(Gas::from_teragas(10)).expect_receipt("echo").clone();
    let out = leaf(r.gas_burnt.checked_sub(r.prepaid_exec_fees).unwrap());
    assert_eq!(out.assert_headroom("echo", Gas::ZERO), Gas::ZERO, "{out}");
}

/// A transaction to its own signer runs its receipt locally, in the
/// conversion block. The log keeps no body for that receipt (nearcore keeps
/// none either): its outcome is described from the transaction.
#[test]
fn a_local_receipt_is_described_from_its_transaction() {
    use near_contract_sim::nearcore::near_parameters::ActionCosts;
    let mut sim = sim_with_users();
    sim.deploy("status.near", status_wasm());
    let args = json!({"message": "hi"}).to_string();
    let out = sim
        .call("status.near", "status.near", "set_status")
        .args(args.clone().into_bytes())
        .gas(Gas::from_teragas(50))
        .transact();
    let r = out.assert_success().receipt("set_status").unwrap();
    assert_eq!(r.block_height, out.transaction.as_ref().unwrap().block_height);
    assert_eq!(r.predecessor_id.as_str(), "status.near");
    assert_eq!(r.attached_gas, Gas::from_teragas(50));
    let exec = |c| sim.config.runtime_config().fees.fee(c).exec_fee().gas.as_gas();
    let fees = exec(ActionCosts::new_action_receipt)
        + exec(ActionCosts::function_call_base)
        + exec(ActionCosts::function_call_byte) * ("set_status".len() + args.len()) as u64;
    assert_eq!(r.prepaid_exec_fees.as_gas(), fees);
}

/// A receipt keeps each call's arguments, a failed call's included: here
/// the amount a transfer to an unregistered account was sent. A receipt
/// that makes no call has none, so `args_json` panics on it.
#[test]
fn a_receipt_keeps_its_calls_arguments() {
    let mut sim = ft_sim();
    let args = json!({"receiver_id": "nobody.near", "amount": "40"});
    let out = sim
        .call("alice.near", "ft.near", "ft_transfer")
        .args_json(&args)
        .deposit(Balance::from_yoctonear(1))
        .transact();
    let r = out.expect_receipt("ft_transfer");
    assert!(r.failure().is_some(), "{out}");
    assert_eq!(r.args, [args.to_string().into_bytes()]);
    assert_eq!(r.args_json::<Value>().unwrap()["amount"], "40");
    let refund = out.receipts.iter().find(|r| r.is_refund()).unwrap();
    assert!(refund.args.is_empty());
    let none = refund.args_json::<Value>().unwrap_err();
    let who = &refund.receiver_id;
    assert_eq!(none.to_string(), format!("{who} makes 0 function calls, not one"));
    let wrong = r.args_json::<u64>().unwrap_err().to_string();
    assert!(wrong.starts_with("ft.near::ft_transfer: invalid type: map"), "{wrong}");
}

/// A batch keeps one args entry per call, in order, so `args_json`, which
/// reads a single call's, is an error on it. A delegate's own receipt makes no
/// call: the call is the receipt it spawns, which keeps its args.
#[test]
fn args_of_a_batch_and_of_a_delegate() {
    let mut sim = sim_with_users();
    sim.deploy("status.near", status_wasm());
    let hi = json!({"message": "hi"});
    let out = sim
        .tx("alice.near", "status.near")
        .call("set_status")
        .args_json(&hi)
        .call("get_status")
        .args_json(json!({"account_id": "alice.near"}))
        .transact();
    let batch = &out.receipts[0];
    assert_eq!(
        out.first_call().map(|r| r.receipt_id),
        Some(batch.receipt_id),
        "the call the signer sent"
    );
    assert_eq!(batch.method_names, ["set_status", "get_status"]);
    assert_eq!(batch.args[0], hi.to_string().into_bytes(), "{out}");
    assert_eq!(batch.args.len(), 2);
    let two = batch.args_json::<Value>().unwrap_err();
    let want = "status.near::set_status,get_status makes 2 function calls, not one";
    assert_eq!(two.to_string(), want);

    let out = sim.call("alice.near", "status.near", "set_status").args_json(&hi).relay("bob.near");
    let outer = out.receipts.iter().find(|r| r.actions == ["Delegate"]).unwrap();
    assert!(outer.args.is_empty(), "{out}");
    let inner = out.expect_receipt("set_status");
    assert_eq!(inner.args_json::<Value>().ok(), Some(hi), "{out}");
}

/// A receipt that does more than call names each action in the tree, and
/// its deposit counts every action that carries one.
#[test]
fn a_receipt_lists_its_actions_and_their_deposits() {
    let mut sim = sim_with_users();
    let gk = SecretKey::from_seed(KeyType::ED25519, "gas").public_key();
    let out = sim
        .tx("alice.near", "alice.near")
        .add_key(gk.clone(), AccessKey::gas_key_full_access(1))
        .transfer_to_gas_key(gk, near(2))
        .deploy(status_wasm())
        .call("set_status")
        .args_json(json!({"message": "hi"}))
        .transfer(near(1))
        .transact();
    let r = &out.assert_success().receipts[0];
    let kinds = ["AddKey", "TransferToGasKey", "DeployContract", "FunctionCall", "Transfer"];
    assert_eq!(r.actions, kinds);
    assert_eq!(r.deposit, near(3));
    let label = "alice.near <- AddKey + TransferToGasKey + DeployContract + set_status + Transfer";
    assert!(out.to_string().contains(&format!("─ #1 {label}  attached ")), "{out}");
}

/// A callback is a receipt that waited on promise results: `promise_then`'s,
/// and a yield's, resumed or timed out. The call it waited on, the receipt
/// that made it, the refunds and the conversion aren't.
#[test]
fn a_callback_waited_on_promise_results() {
    let mut sim = sim_with_users();
    sim.deploy("probe.near", probe_wasm());
    let out = sim.call("alice.near", "probe.near", "call_missing").transact();
    let (refunds, ran): (Vec<_>, Vec<_>) =
        out.transaction.iter().chain(&out.receipts).partition(|r| r.is_refund());
    let ran: Vec<_> = ran.iter().map(|r| (r.method_names.join(""), r.is_callback())).collect();
    let want = [("", false), ("call_missing", false), ("ping", false), ("on_cb", true)];
    assert_eq!(ran, want.map(|(m, c)| (m.to_string(), c)));
    assert!(!refunds.is_empty() && refunds.iter().all(|r| !r.is_callback()));
    let resumed = sim.call("alice.near", "probe.near", "start_yield").transact();
    sim.call("bob.near", "probe.near", "resume").transact().assert_success();
    let timed_out = sim.call("alice.near", "probe.near", "start_yield").transact();
    sim.advance_to(sim.yields()[0].expires_at);
    sim.settle();
    for y in [resumed, timed_out] {
        let y = sim.outcome(y.hash);
        assert!(y.expect_receipt("on_resume").is_callback());
        assert!(!y.expect_receipt("start_yield").is_callback());
    }
}

/// `receipts_in(heights)` lists what ran in those blocks, in execution order,
/// each receipt with its transaction: two calls submitted together run side
/// by side, then their refunds.
#[test]
fn receipts_by_height() {
    let mut sim = sim_with_users();
    sim.deploy("status.near", status_wasm());
    let mark = sim.height();
    let set = |sim: &mut Sim, who| {
        sim.call(who, "status.near", "set_status").args_json(json!({"message": "hi"})).submit()
    };
    let (a, b) = (set(&mut sim, "alice.near"), set(&mut sim, "bob.near"));
    sim.settle();
    let ran: Vec<_> =
        sim.receipts_in(mark + 1..).iter().map(|r| (r.block_height, r.tx, r.is_refund())).collect();
    let (call, refund) = (mark + 2, mark + 3);
    let want = [(call, a, false), (call, b, false), (refund, a, true), (refund, b, true)].to_vec();
    assert_eq!(ran, want);
    let calls: Vec<_> = sim.receipts_in(call..refund).into_iter().map(|r| r.receipt_id).collect();
    let ids = [a, b].map(|h| sim.outcome(h).receipt("set_status").unwrap().receipt_id);
    assert_eq!(calls, ids);
    assert_eq!(sim.outcome(a).transaction.unwrap().tx, a);
}

/// `delete_self(beneficiary)`: a promise to itself whose only action is
/// `DeleteAccount`.
fn deleter_wasm() -> Vec<u8> {
    wat::parse_str(
        r#"(module
  (import "env" "current_account_id" (func $me (param i64)))
  (import "env" "input" (func $input (param i64)))
  (import "env" "register_len" (func $len (param i64) (result i64)))
  (import "env" "read_register" (func $read (param i64 i64)))
  (import "env" "promise_batch_create" (func $create (param i64 i64) (result i64)))
  (import "env" "promise_batch_action_delete_account" (func $delete (param i64 i64 i64)))
  (memory 1)
  (export "memory" (memory 0))
  (func (export "delete_self")
    (call $me (i64.const 0))
    (call $read (i64.const 0) (i64.const 0))
    (call $input (i64.const 1))
    (call $read (i64.const 1) (i64.const 256))
    (call $delete
      (call $create (call $len (i64.const 0)) (i64.const 0))
      (call $len (i64.const 1))
      (i64.const 256))))"#,
    )
    .unwrap()
}

/// A receipt whose only action is `DeleteAccount`, and that waits on no
/// data, is instant, like a yield's `PromiseYield` receipt: nearcore runs it
/// in its parent's block, and it never goes out. The balance it pays the beneficiary is a receipt
/// of its own, in the next block, and to a missing beneficiary it is lost.
#[test]
fn a_delete_account_receipt_runs_in_its_parents_block() {
    let mut sim = sim_with_users();
    sim.deploy("d.near", deleter_wasm());
    let out =
        sim.call("alice.near", "d.near", "delete_self").args(b"nobody.near".to_vec()).transact();
    let parent = out.assert_success().receipt("delete_self").unwrap();
    let child = |of: &near_contract_sim::ReceiptOutcome| {
        out.receipts
            .iter()
            .find(|r| of.receipt_ids.contains(&r.receipt_id) && !r.is_refund())
            .unwrap()
    };
    let delete = child(parent);
    assert_eq!((delete.receiver_id.as_str(), delete.predecessor_id.as_str()), ("d.near", "d.near"));
    assert_eq!(delete.block_height, parent.block_height);
    assert!(sim.account("d.near").is_none());
    let payout = out.receipts.iter().find(|r| delete.receipt_ids.contains(&r.receipt_id)).unwrap();
    assert_eq!(payout.receiver_id.as_str(), "nobody.near");
    assert_eq!(payout.block_height, parent.block_height + 1);
    assert!(payout.is_refund() && payout.unhandled, "{out}");
}

/// A receipt's writes are those nearcore committed for it: `write` stores
/// key `k`, and `write_then_spin` stores it then spins until its gas runs
/// out, so nothing it wrote is committed and it reports none.
#[test]
fn an_out_of_gas_receipt_reports_no_writes() {
    let wasm = wat::parse_str(
        r#"(module
  (import "env" "storage_write" (func $write (param i64 i64 i64 i64 i64) (result i64)))
  (memory 1)
  (export "memory" (memory 0))
  (data (i32.const 0) "kv")
  (func $put (drop (call $write (i64.const 1) (i64.const 0) (i64.const 1) (i64.const 1) (i64.const 0))))
  (func (export "write") (call $put))
  (func (export "write_then_spin") (call $put) (loop (br 0))))"#,
    )
    .unwrap();
    let mut sim = sim_with_users();
    sim.deploy("w.near", wasm);
    let run = |sim: &mut Sim, method: &str| {
        let out = sim.call("alice.near", "w.near", method).gas(Gas::from_teragas(5)).transact();
        let r = out.expect_receipt(method);
        (r.failure(), r.storage_ops().written.iter().map(|w| w.key.clone()).collect::<Vec<_>>())
    };
    let (failure, written) = run(&mut sim.clone(), "write");
    assert_eq!((failure, written), (None, vec![b"k".to_vec()]));
    let (failure, written) = run(&mut sim, "write_then_spin");
    assert_eq!((failure, written), (Some(Failure::OutOfGas), vec![]));
}

/// Each refund says which it is, read off real receipts: unused gas back to
/// the signer, a failed call's deposit back to its sender, and a deleted
/// account's balance to its beneficiary, whether a contract's promise or
/// the transaction itself deleted it.
#[test]
fn refunds_say_which_they_are() {
    use near_contract_sim::nearcore::near_primitives::action::DeleteAccountAction;
    use near_contract_sim::{Action, ReceiptOutcome, Refund, TxOutcome};
    let mut sim = sim_with_users();
    sim.deploy("status.near", status_wasm());
    let refunds = |out: &TxOutcome| -> Vec<(String, Option<Refund>, Balance)> {
        let refunds = out.receipts.iter().filter(|r| r.is_refund());
        refunds.map(|r| (r.receiver_id.to_string(), r.refund, r.deposit)).collect()
    };
    let kinds = |out: &TxOutcome| -> Vec<Refund> {
        out.receipts.iter().filter_map(|r: &ReceiptOutcome| r.refund).collect()
    };

    // A successful call: only gas comes back.
    let ok =
        sim.call("alice.near", "status.near", "set_status").args_json(json!({"message": "hi"}));
    let out = ok.transact();
    out.assert_success();
    assert_eq!(kinds(&out), [Refund::Gas], "{out:#}");
    assert!(out.receipts.iter().all(|r| r.is_refund() == r.refund.is_some()));
    assert!(!out.expect_receipt("set_status").is_refund());

    // A failed call with a deposit: the deposit comes back, and gas.
    let deposit = Balance::from_near(1);
    let out = sim.call("alice.near", "status.near", "no_such_method").deposit(deposit).transact();
    out.assert_failure();
    let back: Vec<_> = refunds(&out).into_iter().filter(|r| r.1 == Some(Refund::Deposit)).collect();
    assert_eq!(back, [("alice.near".to_string(), Some(Refund::Deposit), deposit)], "{out:#}");
    assert!(kinds(&out).contains(&Refund::Gas), "{out:#}");

    // A contract deletes itself through a promise: its balance goes to bob.
    sim.deploy("d.near", deleter_wasm());
    let out = sim.call("alice.near", "d.near", "delete_self").args(b"bob.near".to_vec()).transact();
    out.assert_success();
    let payout = refunds(&out).into_iter().find(|r| r.1 == Some(Refund::AccountDeletion));
    assert_eq!(payout.map(|p| p.0), Some("bob.near".into()), "{out:#}");

    // The transaction itself deletes its signer: the deletion runs as the
    // conversion's local receipt, which has no body of its own.
    sim.create_account("carol.near", Balance::from_near(5));
    let delete =
        Action::DeleteAccount(DeleteAccountAction { beneficiary_id: "bob.near".parse().unwrap() });
    let out = sim.tx("carol.near", "carol.near").action(delete).transact();
    out.assert_success();
    assert!(sim.account("carol.near").is_none());
    let payout = refunds(&out).into_iter().find(|r| r.1 == Some(Refund::AccountDeletion));
    assert_eq!(payout.map(|p| p.0), Some("bob.near".into()), "{out:#}");
}

// ---------------------------------------------------------------------------
// Rendering: `Display` (receipt tree + gas table), appended to assertion panics
// ---------------------------------------------------------------------------

#[test]
fn renders_success() {
    let mut sim = sim_with_users();
    sim.deploy("status.near", status_wasm());
    let out = sim
        .call("alice.near", "status.near", "set_status")
        .args_json(json!({"message": "hi"}))
        .transact();
    assert_rendered(
        &out.to_string(),
        "\
tx 8uiVMk… signed by alice.near at #3 (conversion 0.31 Tgas) -> SUCCESS
└─ #4 status.near::set_status  attached 300.00 Tgas, burnt 1.28 Tgas (0.39 + 0.89 exec fees), headroom 299.61 Tgas  ok
      log: alice.near set_status with message hi
(1 refund receipt(s) not shown)

gas (Tgas)                    wasm    load storage    host    send    exec   total
conversion alice.near            -       -       -       -   0.309       -   0.309
#4 status.near::set_status   0.009   0.071   0.252   0.056       -   0.888   1.277
refunds (not charged)            -       -       -       -       -       -   0.223
total 1.81 Tgas (1809641018049 gas)
",
    );
    // The gas table adds up to the exact total.
    assert!(out.to_string().ends_with(&format!("({} gas)\n", out.gas_burnt().as_gas())));
}

/// A call to a missing account; the callback handles the failure, so the
/// transaction succeeds.
#[test]
fn renders_cross_contract_failure() {
    let mut sim = sim_with_users();
    sim.deploy("fail.near", failure_probe_wasm());
    let out = sim.call("alice.near", "fail.near", "send_to_missing").transact();
    assert_rendered(&out.to_string(), "\
tx 3z2upq… signed by alice.near at #3 (conversion 0.31 Tgas) -> SUCCESS
└─ #4 fail.near::send_to_missing  attached 300.00 Tgas, burnt 1.62 Tgas (0.73 + 0.89 exec fees), headroom 282.49 Tgas  ok (returns a promise)
   ├─ #5 missing.near::ping  attached 5.00 Tgas, burnt 0.89 Tgas (0.00 + 0.89 exec fees)  FAILED: account missing.near does not exist
   └─ #6 fail.near::on_cb [callback of missing.near::ping]  attached 10.00 Tgas, burnt 0.90 Tgas (0.02 + 0.89 exec fees), headroom 9.98 Tgas  ok -> \"cb-failed\"
         log: cb-failed
(4 refund receipt(s) not shown)

gas (Tgas)                       wasm    load storage    host    send    exec   total
conversion alice.near               -       -       -       -   0.309       -   0.309
#4 fail.near::send_to_missing   0.000   0.001       -   0.044   0.689   0.888   1.623
#5 missing.near::ping               -       -       -       -       -   0.888   0.888
#6 fail.near::on_cb             0.000   0.001       -   0.015       -   0.888   0.905
refunds (not charged)               -       -       -       -       -       -   0.893
total 4.62 Tgas (4616926785145 gas)
");
    // `{:#}` shows the refunds too.
    assert_rendered(&format!("{out:#}"), "\
tx 3z2upq… signed by alice.near at #3 (conversion 0.31 Tgas) -> SUCCESS
└─ #4 fail.near::send_to_missing  attached 300.00 Tgas, burnt 1.62 Tgas (0.73 + 0.89 exec fees), headroom 282.49 Tgas  ok (returns a promise)
   ├─ #5 missing.near::ping  attached 5.00 Tgas, burnt 0.89 Tgas (0.00 + 0.89 exec fees)  FAILED: account missing.near does not exist
   │  ├─ #6 fail.near <- transfer 1.0000 NEAR (refund)  burnt 0.22 Tgas  ok
   │  └─ #6 alice.near <- transfer 0.0058 NEAR (refund)  burnt 0.22 Tgas  ok
   ├─ #6 fail.near::on_cb [callback of missing.near::ping]  attached 10.00 Tgas, burnt 0.90 Tgas (0.02 + 0.89 exec fees), headroom 9.98 Tgas  ok -> \"cb-failed\"
   │  │  log: cb-failed
   │  └─ #7 alice.near <- transfer 0.0108 NEAR (refund)  burnt 0.22 Tgas  ok
   └─ #5 alice.near <- transfer 0.2839 NEAR (refund)  burnt 0.22 Tgas  ok

gas (Tgas)                       wasm    load storage    host    send    exec   total
conversion alice.near               -       -       -       -   0.309       -   0.309
#4 fail.near::send_to_missing   0.000   0.001       -   0.044   0.689   0.888   1.623
#5 missing.near::ping               -       -       -       -       -   0.888   0.888
#6 fail.near::on_cb             0.000   0.001       -   0.015       -   0.888   0.905
refunds (not charged)               -       -       -       -       -       -   0.893
total 4.62 Tgas (4616926785145 gas)
");
    let msg = panic_message(std::panic::catch_unwind(|| out.assert_failure()));
    assert_eq!(msg, format!("expected failure; got success -> \"cb-failed\"\n\n{out}"));
    let msg = panic_message(std::panic::catch_unwind(|| out.json::<u64>()));
    let serde = serde_json::from_slice::<u64>(br#""cb-failed""#).unwrap_err();
    assert_eq!(msg, format!("{serde}: {:?}\n\n{out}", r#""cb-failed""#));
}

#[test]
fn renders_out_of_gas() {
    let mut sim = sim_with_users();
    sim.deploy("status.near", status_wasm());
    let out = sim
        .call("alice.near", "status.near", "set_status")
        .args_json(json!({"message": "hi"}))
        .gas(Gas::from_gigagas(200))
        .transact();
    assert_rendered(&out.to_string(), "\
tx 29HYHB… signed by alice.near at #3 (conversion 0.31 Tgas) -> FAILURE: out of gas (Exceeded the prepaid gas)
└─ #4 status.near::set_status  attached 0.20 Tgas, burnt 1.09 Tgas (0.20 + 0.89 exec fees)  FAILED: out of gas (Exceeded the prepaid gas)
      log (rolled back): alice.near set_status with message hi
(1 refund receipt(s) not shown)

gas (Tgas)                    wasm    load storage    host    send    exec   total
conversion alice.near            -       -       -       -   0.309       -   0.309
#4 status.near::set_status   0.007   0.071   0.079   0.042       -   0.888   1.088
refunds (not charged)            -       -       -       -       -       -   0.223
total 1.62 Tgas (1620599473374 gas)
");
    let msg = panic_message(std::panic::catch_unwind(|| {
        out.assert_success();
    }));
    let first = "first failure: #4 status.near::set_status: out of gas (Exceeded the prepaid gas)";
    assert_eq!(msg, format!("expected success; {first}\n\n{out}"));
}

/// Until its block, a transaction is only queued: nothing has run, so it has
/// no conversion outcome and renders as one line.
#[test]
fn renders_a_queued_transaction() {
    let mut sim = sim_with_users();
    let h = sim.call("alice.near", "bob.near", "hello").submit();
    let out = sim.outcome(h);
    assert!(out.transaction.is_none() && out.receipts.is_empty() && out.is_pending());
    assert_eq!(out.status, FinalExecutionStatus::NotStarted);
    assert_rendered(&out.to_string(), "tx AxkM3j… -> QUEUED (not in a block yet)\n");
    sim.step();
    let out = sim.outcome(h);
    assert!(out.transaction.is_some() && out.is_pending());
    assert_eq!(out.status, FinalExecutionStatus::Started);
}

/// `key::borsh` reads the near-sdk token's balance map: the prefix its
/// `FungibleToken` was given, then the holder's id in borsh.
#[test]
fn a_near_sdk_map_entry_reads_by_its_key() {
    let sim = ft_sim();
    let alice: near_contract_sim::AccountId = "alice.near".parse().unwrap();
    let entries = sim.view_state("ft.near", b"");
    let prefix = (entries.iter())
        .find_map(|(k, _)| k.strip_suffix(&borsh::to_vec(&alice).unwrap()[..]))
        .expect("an entry keyed by alice's id")
        .to_vec();
    let held = sim.state("ft.near", &near_contract_sim::key::borsh(&prefix, &alice));
    assert_eq!(held, Some(1000u128.to_le_bytes().to_vec()), "{prefix:?}");
}

// ---------------------------------------------------------------------------
// Failure output: the headline, a missing method, long trees, rolled-back logs
// ---------------------------------------------------------------------------

/// The message's first line names the receipt that failed first, in plain
/// text: here the forced hop, not the callbacks that failed after it.
#[test]
fn assert_success_opens_with_the_first_failure() {
    use near_contract_sim::{Fault, Receipts};
    let mut sim = sim_with_users();
    sim.deploy("xcc.near", xcc_wasm());
    let out = sim
        .call("alice.near", "xcc.near", "factorial")
        .args_json(json!({"n": 4}))
        .inject(Receipts::method("factorial").nth(1), Fault::OutOfGas)
        .transact();
    let first = out.receipts.iter().find(|r| r.failure().is_some()).expect("a failure");
    let want = format!(
        "expected success; first failure: #{} xcc.near::factorial: out of gas (Exceeded the \
         prepaid gas)",
        first.block_height
    );
    for msg in [
        panic_message(std::panic::catch_unwind(|| {
            out.assert_success();
        })),
        panic_message(std::panic::catch_unwind(|| out.json::<Value>())),
        panic_message(std::panic::catch_unwind(|| out.u128())),
    ] {
        assert_eq!(msg.lines().next(), Some(want.as_str()), "{msg}");
        assert!(!msg.contains("ActionError"), "{msg}");
        assert!(msg.ends_with(&format!("\n\n{out}")), "{msg}");
    }
}

/// A transaction refused at conversion names the conversion; a success
/// asserted to fail shows what it returned.
#[test]
fn the_headline_of_a_refused_transaction_and_of_a_success() {
    let mut sim = sim_with_users();
    sim.deploy("fail.near", failure_probe_wasm());
    let poor =
        sim.call("alice.near", "fail.near", "hello").deposit(Balance::from_near(5_000)).transact();
    let msg = panic_message(std::panic::catch_unwind(|| {
        poor.assert_success();
    }));
    let line = msg.lines().next().unwrap_or_default();
    let height = poor.transaction.as_ref().map(|t| t.block_height).unwrap_or_default();
    let want = format!(
        "expected success; first failure: #{height} conversion of a tx signed by alice.near: invalid transaction: "
    );
    assert!(line.starts_with(&want), "{msg}");
    let ok = sim.call("alice.near", "fail.near", "send_to_missing").transact();
    let msg = panic_message(std::panic::catch_unwind(|| ok.assert_failure()));
    assert_eq!(
        msg.lines().next(),
        Some(r#"expected failure; got success -> "cb-failed""#),
        "{msg}"
    );
}

/// A call or view of a method the contract doesn't export lists the ones
/// it does.
#[test]
fn a_missing_method_lists_the_contracts_methods() {
    let mut sim = sim_with_users();
    sim.deploy("xcc.near", xcc_wasm());
    let methods = "xcc.near has no method factorail; it exports contract_source_metadata, \
                   factorial, factorial_mult";
    let out = sim.call("alice.near", "xcc.near", "factorail").transact();
    let msg = panic_message(std::panic::catch_unwind(|| {
        out.assert_success();
    }));
    assert!(msg.lines().next().is_some_and(|l| l.ends_with(methods)), "{msg}");
    assert!(out.to_string().contains(&format!("FAILED: MethodNotFound: {methods}\n")), "{out}");
    let view = sim.view("xcc.near", "factorail", ());
    let msg = panic_message(std::panic::catch_unwind(|| view.json::<Value>()));
    assert!(msg.ends_with(methods), "{msg}");
}

/// A deep chain of the same receipts collapses into a line, so a failing
/// test prints at most 100 lines; `{:#}` still shows every receipt.
#[test]
fn a_long_tree_collapses_repeated_receipts() {
    let mut sim = sim_with_users();
    sim.deploy("xcc.near", xcc_wasm());
    let out =
        sim.call("alice.near", "xcc.near", "factorial").args_json(json!({"n": 200})).transact();
    let msg = panic_message(std::panic::catch_unwind(|| {
        out.assert_success();
    }));
    let first = out.receipts.iter().find(|r| r.failure().is_some()).expect("a failure");
    assert!(msg.lines().count() <= 100, "{} lines:\n{msg}", msg.lines().count());
    // The first failure is still in the tree, and in the gas table.
    let label = format!("#{} xcc.near::factorial ", first.block_height);
    assert_eq!(msg.matches(&label).count(), 2, "{msg}");
    assert!(msg.contains(" more xcc.near::factorial"), "{msg}");
    let full = format!("{out:#}");
    let calls = out.receipts.iter().filter(|r| !r.is_refund()).count();
    assert!(full.lines().count() > 2 * calls, "{full}");
    // The table's rows still add up to the total.
    assert!(msg.ends_with(&format!("({} gas)\n", out.gas_burnt().as_gas())), "{msg}");
}

/// A failed receipt's logs are shown, marked as rolled back with it.
#[test]
fn a_failed_receipts_logs_are_marked_rolled_back() {
    let mut sim = sim_with_users();
    sim.deploy("status.near", status_wasm());
    let out = sim
        .call("alice.near", "status.near", "set_status")
        .args_json(json!({"message": "hi"}))
        .gas(Gas::from_gigagas(200))
        .transact();
    let t = out.to_string();
    assert!(
        t.contains("\n      log (rolled back): alice.near set_status with message hi\n"),
        "{t}"
    );
    let ok = sim
        .call("alice.near", "status.near", "set_status")
        .args_json(json!({"message": "hi"}))
        .transact();
    assert!(ok.to_string().contains("\n      log: alice.near set_status"), "{ok}");
}

/// No findings say so, rather than print nothing.
#[test]
fn no_findings_say_so() {
    let sim = sim_with_users();
    assert_eq!(sim.findings().to_string(), "no findings");
}

/// A near-sdk `U128` read as a number says to use `.u128()`.
#[test]
fn a_u128_string_read_as_a_number_points_at_u128() {
    let sim = ft_sim();
    let view = sim.view("ft.near", "ft_balance_of", json!({"account_id": "alice.near"}));
    assert_eq!(view.u128(), 1000);
    let msg = panic_message(std::panic::catch_unwind(|| view.json::<u128>()));
    assert!(msg.contains("near-sdk's U128 or U64 is a JSON string: read it with .u128()"), "{msg}");
    let mut sim = sim;
    let out = sim
        .call("alice.near", "ft.near", "ft_balance_of")
        .args_json(json!({"account_id": "alice.near"}))
        .transact();
    let msg = panic_message(std::panic::catch_unwind(|| out.json::<u64>()));
    assert!(msg.contains("near-sdk's U128 or U64 is a JSON string: read it with .u128()"), "{msg}");
}

/// `assert_panic_contains` is `#[should_panic(expected = ..)]`: a fragment
/// of the panic's message. `Failure::message` is that message.
#[test]
fn assert_panic_contains_matches_a_fragment() {
    let mut sim = sim_with_users();
    sim.deploy("fail.near", failure_probe_wasm());
    let out = sim.call("alice.near", "fail.near", "write_then_panic").transact();
    assert_eq!(out.assert_panic_contains("oo"), "boom");
    assert_eq!(out.assert_failure().message(), Some("boom"));
    assert_eq!(Failure::Host("trap".into()).message(), Some("trap"));
    assert_eq!(Failure::OutOfGas.message(), None);
    let msg = panic_message(std::panic::catch_unwind(|| out.assert_panic_contains("bang")));
    let want = r#"expected a panic containing "bang"; it failed with: panicked: boom"#;
    assert_eq!(msg.lines().next(), Some(want), "{msg}");
    // The failure compared is the transaction's; the first, if another, follows.
    sim.deploy("xcc.near", xcc_wasm());
    let out = sim
        .call("alice.near", "xcc.near", "factorial")
        .args_json(json!({"n": 4}))
        .inject(
            near_contract_sim::Receipts::method("factorial").nth(1),
            near_contract_sim::Fault::OutOfGas,
        )
        .transact();
    let msg = panic_message(std::panic::catch_unwind(|| out.assert_panic_contains("nope")));
    let first = msg.lines().next().unwrap_or_default();
    let want = "it failed with: panicked: Callback computation 0 was not successful; first \
                failure: #";
    assert!(first.contains(want), "{msg}");
    assert!(
        first.ends_with(" xcc.near::factorial: out of gas (Exceeded the prepaid gas)"),
        "{msg}"
    );
    let ok = sim.call("alice.near", "fail.near", "send_to_missing").transact();
    let msg = panic_message(std::panic::catch_unwind(|| ok.assert_panic_contains("x")));
    let want = r#"expected a panic containing "x"; got success -> "cb-failed""#;
    assert_eq!(msg.lines().next(), Some(want), "{msg}");
}

/// A view reads as a call's outcome does, and a `Failure` is an `Error`.
#[test]
fn a_view_reads_as_an_outcome_does() -> Result<(), Box<dyn std::error::Error>> {
    let mut sim = sim_with_users();
    sim.deploy("status.near", status_wasm());
    let set = json!({"message": "hi"});
    sim.call("alice.near", "status.near", "set_status").args_json(set).transact().assert_success();
    let v = sim.view("status.near", "get_status", json!({"account_id": "alice.near"}));
    assert!(v.is_success() && !v.is_failure() && v.failure().is_none());
    assert_eq!(v.assert_success().bytes(), br#""hi""#);
    assert_eq!(v.value(), Some(&br#""hi""#[..]));
    assert_eq!(v.logs(), ["get_status for account_id alice.near"]);
    let bad = sim.view("status.near", "get_status", json!({}));
    assert!(bad.is_failure() && bad.value().is_none());
    let msg = panic_message(std::panic::catch_unwind(|| bad.bytes().to_vec()));
    assert!(
        msg.starts_with("view status.near.get_status at #") && msg.contains(" failed: panicked: "),
        "{msg}"
    );
    assert!(bad.assert_panic_contains("deserialize").contains("deserialize"));
    assert!(!bad.is_success() && bad.failure() == Some(bad.assert_failure()));
    let msg = panic_message(std::panic::catch_unwind(|| bad.assert_panic_contains("nope")));
    assert!(msg.contains(r#"expected a panic containing "nope"; it failed with: "#), "{msg}");
    let e: Box<dyn std::error::Error> = bad.assert_failure().into();
    assert!(e.to_string().starts_with("panicked: "), "{e}");
    let bytes = v.result?;
    assert_eq!(bytes, br#""hi""#);
    Ok(())
}

/// A call to an account with no contract says so in plain text, not
/// nearcore's `Debug`.
#[test]
fn a_call_to_an_account_without_code_says_so() {
    let mut sim = sim_with_users();
    let out = sim.call("alice.near", "bob.near", "go").transact();
    let msg = panic_message(std::panic::catch_unwind(|| {
        out.assert_success();
    }));
    let first = msg.lines().next().unwrap_or_default();
    assert!(
        first.ends_with("bob.near::go: CodeDoesNotExist: bob.near has no contract deployed"),
        "{msg}"
    );
    assert!(!msg.contains("AccountId("), "{msg}");
    let view = sim.view("bob.near", "go", ());
    assert_eq!(
        view.assert_failure().to_string(),
        "CodeDoesNotExist: bob.near has no contract deployed"
    );
}

/// A call delivered from another predecessor says so in the headline, as
/// the tree marks it: the guard it failed is about who called.
#[test]
fn the_headline_names_a_delivered_calls_predecessor() {
    let mut sim = sim_with_users();
    sim.deploy("vault.near", res("vault.wasm"));
    sim.call("vault.near", "vault.near", "new")
        .args_json(json!({ "token_id": "token.near" }))
        .transact()
        .assert_success();
    let out = sim
        .call("alice.near", "vault.near", "ft_on_transfer")
        .args_json(json!({ "sender_id": "alice.near", "amount": "100", "msg": "" }))
        .predecessor("evil.near")
        .transact();
    let msg = panic_message(std::panic::catch_unwind(|| {
        out.assert_success();
    }));
    let first = msg.lines().next().unwrap_or_default();
    let want = " vault.near::ft_on_transfer [delivered from evil.near]: panicked: only the \
                vault's token deposits";
    assert!(
        first.starts_with("expected success; first failure: #") && first.ends_with(want),
        "{msg}"
    );
}

/// A missing method's exports are the code the receipt ran against: a
/// later deploy doesn't change an old outcome read again.
#[test]
fn a_missing_methods_exports_are_the_code_it_ran_against() {
    let mut sim = sim_with_users();
    sim.deploy("c.near", status_wasm());
    let out = sim.call("alice.near", "c.near", "factorial").transact();
    let then = out.to_string();
    assert!(then.contains("c.near has no method factorial; it exports "), "{then}");
    sim.deploy("c.near", xcc_wasm());
    let again = sim.outcome(out.hash).to_string();
    assert!(!again.contains("factorial_mult"), "{again}");
    assert_eq!(again, then);
}
