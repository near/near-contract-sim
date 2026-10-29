//! The replay store: each finding of a scan or a search is one
//! JSON file with its plan as it ran, its label and the fixture's
//! fingerprint; `Store::regressions` reproduces every file on a harness,
//! in a fresh process too. On the fixed build a replay reads `Gone`, the
//! fixture marked changed; a replay that can't be judged reads `Stale`,
//! never a panic; one gone on its own fixture names a changed harness. Real WASM (the vault specimen) throughout.

mod common;
mod explore_vault;
mod fuzz_common;

use explore_vault::{User, VaultOp, fails_after_ops, vault_on};
use near_contract_sim::Receipts;
use near_contract_sim::explore::{Amount, Faults, Harness, Plan, Regression, Status, gas, plans};
use near_contract_sim::{Fault, Fixture, Sim, builtin};
use proptest::prelude::*;
use std::path::PathBuf;

const PAYOUT: &str = "withdraw delivers what it debits";

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ncs-replay-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn deposit_then_withdraw() -> Plan<VaultOp> {
    Plan::new()
        .op(VaultOp::Deposit { who: User::Alice, amount: Amount::Exact(100) })
        .op(VaultOp::Withdraw { who: User::Alice, amount: Amount::Exact(40) })
}

/// The forgetful payout, found by a scan with the payout failed, saved in
/// `dir`: its replay file.
fn forgetful_replay(dir: &PathBuf) -> PathBuf {
    let harness = vault_on("vault_forgetful.wasm").store(dir);
    let audit = harness.scan(&[deposit_then_withdraw()]).faults(Faults::tolerable()).run();
    let found = audit.assert_found(PAYOUT, "Withdraw");
    found.replay.clone().unwrap_or_else(|| panic!("no replay saved:\n{audit}"))
}

fn status(r: &Regression<VaultOp>) -> &'static str {
    match r.status {
        Status::Shows(_) => "shows",
        Status::Gone(_) => "gone",
        Status::NotReached(_) => "not reached",
        Status::Stale(_) => "stale",
    }
}

/// Every finding is a file; every file reproduces on the build that found
/// it, and reads `Gone` on the fixed one, its fixture marked changed.
#[test]
fn each_finding_is_a_replay_and_each_replay_a_regression() {
    let dir = temp_dir("each");
    let path = forgetful_replay(&dir);
    let json: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(json["check"], PAYOUT);
    assert_eq!(json["label"], "forced (PromiseFailed at vault.near::on_withdraw)");
    assert!(json["fixture"].is_string(), "{json}");
    let inject = &json["plan"]["steps"][1]["inject"][0]["at"];
    assert!(inject.to_string().contains("on_withdraw"), "the receipt it landed on: {json}");
    let format = vault_on("vault.wasm").union(&[]).format;
    assert_eq!(json["format"], format, "a format version: {json}");
    let files = std::fs::read_dir(&dir).unwrap().count();
    let again = vault_on("vault_forgetful.wasm").store(&dir).regressions();
    assert_eq!(again.len(), files);
    let payout = again.iter().find(|r| r.path == path).expect("its regression");
    assert_eq!(status(payout), "shows", "{payout:?}");
    assert!(!payout.fixture_changed);
    // A file written before versions (no `format`) reads as version 0: an
    // older identity scheme, stale, never judged on a misreading.
    let mut old = json.clone();
    old.as_object_mut().unwrap().remove("format");
    std::fs::write(&path, old.to_string()).unwrap();
    let again = vault_on("vault_forgetful.wasm").store(&dir).regressions();
    let payout = again.iter().find(|r| r.path == path).expect("its regression");
    assert_eq!(status(payout), "stale", "{payout:?}");
    // One written by a newer format is stale, not judged on a misreading.
    let mut newer = json.clone();
    newer["format"] = (format + 1).into();
    std::fs::write(&path, newer.to_string()).unwrap();
    let again = vault_on("vault_forgetful.wasm").store(&dir).regressions();
    let payout = again.iter().find(|r| r.path == path).expect("its regression");
    assert_eq!(status(payout), "stale", "{payout:?}");
    std::fs::write(&path, json.to_string()).unwrap();
    let fixed = vault_on("vault.wasm").store(&dir).regressions();
    let payout = fixed.iter().find(|r| r.path == path).expect("its regression");
    assert_eq!(status(payout), "gone", "{payout:?}");
    assert!(payout.fixture_changed);
    let _ = std::fs::remove_dir_all(dir);
}

/// A replay whose plan no longer reads is stale; reproducing it never
/// panics. One saved before replays named their harness's checks, whose
/// check the harness lacks, is stale too.
#[test]
fn a_replay_that_cant_be_judged_is_stale() {
    let dir = temp_dir("stale");
    let path = forgetful_replay(&dir);
    // The same fixture and check names, its check changed to pass: gone,
    // but the contract is the one it was saved on, so the harness changed,
    // not the bug.
    let names = ["total_shares is the sum of shares", "the vault holds what it owes", PAYOUT];
    let names = names.into_iter().chain(["deposit credits what it takes"]);
    let changed = names.fold(
        Harness::<VaultOp>::new(explore_vault::vault_world("vault_forgetful.wasm")),
        |h, name| h.at_rest(name, |_: &Sim| Ok::<(), String>(())),
    );
    let r = changed.clone().store(&dir).regressions().into_iter().find(|r| r.path == path).unwrap();
    assert!(r.harness_changed() && !r.fixture_changed, "{r:?}");
    let same = vault_on("vault_forgetful.wasm").store(&dir).regressions();
    assert!(same.iter().all(|r| !r.harness_changed()), "{same:?}");
    let mut json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    json["plan"]["steps"][0]["op"] = serde_json::json!({ "Renamed": {} });
    std::fs::write(&path, json.to_string()).unwrap();
    let renamed = vault_on("vault_forgetful.wasm").store(&dir).regressions();
    let r = renamed.iter().find(|r| r.path == path).unwrap();
    assert!(matches!(&r.status, Status::Stale(why) if why.contains("no longer reads")), "{r:?}");
    // An older file, without its harness's checks, on a harness without its check.
    json.as_object_mut().unwrap().remove("checks");
    std::fs::write(&path, json.to_string()).unwrap();
    let other = Harness::<VaultOp>::new(explore_vault::vault_world("vault_forgetful.wasm"));
    let unchecked = other.clone().store(&dir).regressions();
    let r = unchecked.iter().find(|r| r.path == path).unwrap();
    assert!(matches!(&r.status, Status::Stale(why) if why.contains(PAYOUT)), "{r:?}");
    let _ = std::fs::remove_dir_all(dir);
}

/// Two harnesses share a store. A replay is the harness's that saved it,
/// as its checks: one with every one of them judges it, and one without
/// (another harness, here one lacking an extra check) neither reproduces
/// it nor lists it as stale.
///
/// Verification boundary: which replays `Store::regressions` and a scan's
/// open list take, over actual runs.
#[test]
fn another_harness_s_replays_are_neither_regressions_nor_stale() {
    let dir = temp_dir("shared");
    // A check that always fails: its replays name a check the other lacks.
    let extra = |h: Harness<VaultOp>| h.at_rest("an extra check", fails_after_ops("by design"));
    let theirs = extra(vault_on("vault_forgetful.wasm")).store(&dir);
    let audit = theirs.scan(&[deposit_then_withdraw()]).faults(Faults::tolerable()).run();
    let path = audit.assert_found(PAYOUT, "Withdraw").replay.clone().expect("saved");
    audit.assert_found("an extra check", "world");
    let files = std::fs::read_dir(&dir).unwrap().count();
    assert_eq!(theirs.clone().store(&dir).regressions().len(), files);
    // A harness without the extra check: none of them is its.
    let mine = vault_on("vault_forgetful.wasm").store(&dir);
    assert!(mine.clone().store(&dir).regressions().is_empty());
    // The replays of a check the other harness has and this one lacks
    // aren't stale here.
    let mine = mine.at_rest("a check of mine", |_: &Sim| Ok::<(), String>(()));
    let scan = mine.scan(&[deposit_then_withdraw()]).faults(Faults::none()).run();
    assert!(scan.todo().iter().all(|t| !t.id.starts_with("stale:")), "{scan}");
    // A harness with more checks than the one that saved them judges them.
    let more =
        extra(vault_on("vault_forgetful.wasm")).at_rest("one more", |_: &Sim| Ok::<(), String>(()));
    let judged = more.clone().store(&dir).regressions();
    assert!(judged.iter().any(|r| r.path == path && status(r) == "shows"), "{judged:?}");
    let _ = std::fs::remove_dir_all(dir);
}

/// A replay reproduces in a fresh process: nothing cached from the scan.
#[test]
fn a_replay_reproduces_in_a_fresh_process() {
    let dir = temp_dir("fresh");
    forgetful_replay(&dir);
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "fresh_reproduce_child", "--include-ignored", "--nocapture"])
        .env("NCS_REPLAY_DIR", &dir)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "child failed:\n{stdout}\n{stderr}");
    assert!(stdout.contains("CHILD OK"), "child didn't run:\n{stdout}");
    let _ = std::fs::remove_dir_all(dir);
}

/// Runs only from `a_replay_reproduces_in_a_fresh_process`.
#[test]
#[ignore = "child process of a_replay_reproduces_in_a_fresh_process"]
fn fresh_reproduce_child() {
    let Ok(dir) = std::env::var("NCS_REPLAY_DIR") else { return };
    let all = vault_on("vault_forgetful.wasm").store(dir).regressions();
    let payout = all.iter().find(|r| r.check == PAYOUT).expect("the payout's replay");
    let Status::Shows(run) = &payout.status else { panic!("{payout:?}") };
    let f = run.assert_found(PAYOUT, "Withdraw");
    assert!(f.message.contains("gave up 40 shares and received 0 tokens"), "{run}");
    println!("CHILD OK");
}

/// A search leaves one replay per row it reports, its case as reported,
/// a sampled fault saved as the receipt it landed on: not the cases it
/// shrank, nor its saves file's snapshots, which came to about 1000
/// files a specimen. A file that was there before stays.
///
/// Verification boundary: the files a search leaves, over an actual
/// search with saves.
#[test]
fn a_search_leaves_one_replay_per_row() {
    let dir = temp_dir("search");
    std::fs::create_dir_all(&dir).unwrap();
    let before = dir.join("before.json");
    std::fs::write(&before, "{}").unwrap();
    let saves = temp_dir("search-saves").join("saves.json");
    let sim: Sim = Fixture::new()
        .account("alice.near", near_contract_sim::Balance::from_near(10))
        .account("missing.near", near_contract_sim::Balance::from_near(1))
        .contract("contract.near", fuzz_common::fuzz_wasm())
        .build();
    /// A detached call to an account with no code: an unhandled failure.
    #[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
    struct Missing;
    impl near_contract_sim::explore::Op for Missing {
        fn submit(&self, sim: &mut Sim) -> Result<(), near_contract_sim::explore::Skip> {
            use fuzz_common::{Op as Fz, TGAS, encode};
            let call = Fz::Call {
                to: "missing.near".into(),
                args: encode(&[]),
                deposit: 0,
                gas: 10 * TGAS,
                weight: 0,
            };
            sim.call("alice.near", "contract.near", "run").args(encode(&[call])).submit();
            Ok(())
        }
    }
    let strategy = plans(Just(Missing))
        .len(1..=2)
        .gas(gas::off())
        .faults(Faults::new(Receipts::all(), [Fault::Hold(1)]).per_case(0.5));
    let audit =
        Harness::new(sim).store(&dir).search(strategy).seed(4).cases(16).saves(&saves, 2).run();
    let found = audit.assert_found(builtin::UNHANDLED_FAILURE, "missing.near::run");
    assert!(found.replay.as_ref().is_some_and(|p| p.exists()), "{audit}");
    // Its sites are its own receipt's, and a call to no code reaches none:
    // not contract.near::run, which ran before it.
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(found.replay.as_ref().unwrap()).unwrap()).unwrap();
    assert_eq!(saved["sites"], serde_json::json!([]), "{saved}");
    let json = |p: &Plan<Missing>| serde_json::to_string(p).unwrap();
    let shrunk = audit.found().iter().any(|x| json(x.run.plan()) != json(x.original.plan()));
    assert!(shrunk, "a shrunk row, its first case saved and removed: {audit}");
    let mut files: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).collect();
    files.sort();
    let mut rows: Vec<_> = audit.found().iter().map(|x| x.replay.clone().unwrap()).collect();
    rows.push(before);
    rows.sort();
    assert_eq!(files, rows, "{audit}");
    let plans: Vec<serde_json::Value> = (files.iter().filter(|p| !p.ends_with("before.json")))
        .map(|p| serde_json::from_slice::<serde_json::Value>(&std::fs::read(p).unwrap()).unwrap())
        .map(|v| v["plan"].clone())
        .collect();
    // A sample is saved resolved: an exact receipt, never `nth` alone.
    for plan in &plans {
        let injects =
            plan["steps"].as_array().unwrap().iter().flat_map(|s| {
                s.get("inject").and_then(|i| i.as_array()).cloned().unwrap_or_default()
            });
        for at in injects.map(|i| i["at"].clone()) {
            assert!(at["nth"].is_null() || at["exact"] == true, "a sample saved: {plan}");
        }
    }
    let _ = std::fs::remove_dir_all(dir);
    let _ = std::fs::remove_dir_all(saves.parent().unwrap());
}

/// One run saves nothing, though its harness has a store, so a sweep of
/// runs doesn't fill a campaign's store; a scan of its plan does.
/// A finding's replays from two builds (one wasm with a custom section
/// appended: other code, the same behavior) are two files, so a scan
/// on another build never overwrites the audited build's replay.
#[test]
fn a_run_saves_nothing_and_two_builds_keep_their_replays() {
    let dir = temp_dir("builds");
    let audit = vault_on("vault_forgetful.wasm")
        .scan(&[deposit_then_withdraw()])
        .faults(Faults::tolerable())
        .run();
    let plan = audit.assert_found(PAYOUT, "Withdraw").run.plan().clone();
    let run = vault_on("vault_forgetful.wasm").store(&dir).run(&plan);
    assert!(run.findings().iter().any(|f| f.check == PAYOUT), "{run}");
    assert!(!dir.exists(), "a run saved");
    let mut other = common::res("vault_forgetful.wasm");
    other.extend([0, 2, 1, b'x']);
    let builds = [
        vault_on("vault_forgetful.wasm"),
        explore_vault::harness(explore_vault::vault_world_with(other)),
    ];
    for h in &builds {
        let h = h.clone().store(&dir);
        let audit = h.scan(std::slice::from_ref(&plan)).faults(Faults::none()).run();
        audit.assert_found(PAYOUT, "Withdraw");
    }
    let saved = builds[0].clone().store(&dir).regressions();
    let payouts: Vec<_> = saved.iter().filter(|r| r.check == PAYOUT).collect();
    assert_eq!(payouts.len(), 2, "{saved:#?}");
    assert_eq!(payouts.iter().filter(|r| r.fixture_changed).count(), 1, "{saved:#?}");
    let _ = std::fs::remove_dir_all(dir);
}

/// A replay that no longer shows says why: on the fixed vault its
/// breaking receipt's site (`on_withdraw`) still runs and the check holds
/// (`Gone`); on a world where alice gave her tokens away the withdraw is
/// refused before it, so the site isn't reached (`NotReached`).
#[test]
fn a_gone_replay_says_whether_its_site_was_reached() {
    let dir = temp_dir("gone");
    let path = forgetful_replay(&dir);
    let mine = |rs: Vec<Regression<VaultOp>>| rs.into_iter().find(|r| r.path == path).unwrap();
    let fixed = mine(vault_on("vault.wasm").store(&dir).regressions());
    assert_eq!(status(&fixed), "gone", "{fixed:?}");
    let mut poor = explore_vault::vault_world("vault_forgetful.wasm");
    let away = serde_json::json!({ "receiver_id": "bob.near", "amount": "1000" });
    let one = near_contract_sim::Balance::from_yoctonear(1);
    poor.call("alice.near", explore_vault::TOKEN, "ft_transfer")
        .args_json(away)
        .deposit(one)
        .transact()
        .assert_success();
    let absent = mine(explore_vault::harness(poor).store(&dir).regressions());
    assert_eq!(status(&absent), "not reached", "{absent:?}");
    let _ = std::fs::remove_dir_all(dir);
}

/// A replay whose forced fault fits no receipt on the build it runs on
/// (here its occurrence moved past the one `on_withdraw` call) forced
/// nothing: it reads `NotReached`, never `Gone`, though its sites ran.
///
/// Verification boundary: replay judgment over an actual run whose
/// injection applied to no receipt.
#[test]
fn a_replay_whose_fault_lands_nowhere_is_not_reached() {
    let dir = temp_dir("no-landing");
    let path = forgetful_replay(&dir);
    let mut json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let at = &mut json["plan"]["steps"][1]["inject"][0]["at"];
    assert_eq!(at["nth"], 0, "{at}");
    at["nth"] = 7.into();
    std::fs::write(&path, json.to_string()).unwrap();
    let all = vault_on("vault.wasm").store(&dir).regressions();
    let moved = all.iter().find(|r| r.path == path).expect("its regression");
    assert_eq!(status(moved), "not reached", "{moved:?}");
    let _ = std::fs::remove_dir_all(dir);
}

/// `go` writes `bad`; the other build has no `go`.
const GOES: &str = r#"(module
  (import "env" "storage_write" (func $sw (param i64 i64 i64 i64 i64) (result i64)))
  (memory 1)
  (data (i32.const 0) "bad")
  (func (export "go")
    (drop (call $sw (i64.const 3) (i64.const 0) (i64.const 3) (i64.const 0) (i64.const 1)))))"#;
const NO_GO: &str = r#"(module (memory 1) (func (export "other")))"#;

/// A replay whose path doesn't exist on another build (its method isn't
/// exported there) reads `NotReached`, never `Gone`:
/// the call failing to resolve reached nothing.
#[test]
fn a_replay_whose_method_is_gone_is_not_reached() {
    let dir = temp_dir("no-method");
    let on = |wat: &str| {
        let sim = Fixture::new()
            .account("alice.near", near_contract_sim::Balance::from_near(10))
            .contract("c.near", ::wat::parse_str(wat).unwrap())
            .build();
        Harness::<VaultOp>::new(sim).at_rest("nothing bad", |sim: &Sim| {
            match sim.state("c.near", b"bad") {
                Some(_) => Err("bad was written".to_owned()),
                None => Ok(()),
            }
        })
    };
    let plan =
        Plan::new().call(near_contract_sim::explore::Call::new("alice.near", "c.near", "go"));
    let audit = on(GOES).store(&dir).scan(&[plan]).faults(Faults::none()).run();
    assert!(!audit.found().is_empty(), "{audit}");
    let all = on(NO_GO).store(&dir).regressions();
    assert_eq!(all.len(), 1, "{all:#?}");
    assert_eq!(status(&all[0]), "not reached", "{:?}", all[0]);
    let _ = std::fs::remove_dir_all(dir);
}

/// `a` and `b` each write `bad`; the fixed build's `a` writes nothing.
const A_AND_B: &str = r#"(module
  (import "env" "storage_write" (func $sw (param i64 i64 i64 i64 i64) (result i64)))
  (memory 1)
  (data (i32.const 0) "bad")
  (func (export "a")
    (drop (call $sw (i64.const 3) (i64.const 0) (i64.const 3) (i64.const 0) (i64.const 1))))
  (func (export "b")
    (drop (call $sw (i64.const 3) (i64.const 0) (i64.const 3) (i64.const 0) (i64.const 1)))))"#;
const ONLY_B: &str = r#"(module
  (import "env" "storage_write" (func $sw (param i64 i64 i64 i64 i64) (result i64)))
  (memory 1)
  (data (i32.const 0) "bad")
  (func (export "a"))
  (func (export "b")
    (drop (call $sw (i64.const 3) (i64.const 0) (i64.const 3) (i64.const 0) (i64.const 1)))))"#;

/// One plan, two causes at one check and site: `a` breaks
/// it, and with `a` fixed, `b` does. The replay saved for `a`'s cause
/// reads `Gone` on the build that fixed `a`, though `b`'s finding shows
/// at the same check and site. A file saved before replays kept the
/// identity matches on check and site alone, and says so.
///
/// Verification boundary: replay judgment by finding identity, over an
/// actual run on each build.
#[test]
fn a_replay_matches_its_own_cause_not_another_at_its_site() {
    let dir = temp_dir("two-causes");
    let on = |wat: &str| {
        let sim = Fixture::new()
            .account("alice.near", near_contract_sim::Balance::from_near(10))
            .contract("c.near", ::wat::parse_str(wat).unwrap())
            .build();
        Harness::<VaultOp>::new(sim).at_rest("nothing bad", |sim: &Sim| {
            match sim.state("c.near", b"bad") {
                Some(_) => Err("bad was written".to_owned()),
                None => Ok(()),
            }
        })
    };
    let call = |m: &str| near_contract_sim::explore::Call::new("alice.near", "c.near", m);
    let plan = Plan::new().call(call("a")).call(call("b"));
    let broken = on(A_AND_B).store(&dir);
    let audit = broken.scan(std::slice::from_ref(&plan)).faults(Faults::none()).run();
    assert_eq!(audit.found().len(), 1, "{audit}");
    let fixed = on(ONLY_B);
    assert!(!fixed.run(&plan).findings().is_empty(), "b still breaks it");
    let all = fixed.clone().store(&dir).regressions();
    assert_eq!(all.len(), 1, "{all:#?}");
    assert_eq!(status(&all[0]), "gone", "{:?}", all[0]);
    assert!(!all[0].by_site);

    // The same file saved before identities: matched by site, and says so.
    let path = all[0].path.clone();
    let mut json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert!(json.as_object_mut().unwrap().remove("id").is_some(), "{json}");
    std::fs::write(&path, json.to_string()).unwrap();
    let old = fixed.clone().store(&dir).regressions();
    assert_eq!((status(&old[0]), old[0].by_site), ("shows", true), "{:?}", old[0]);
    let _ = std::fs::remove_dir_all(dir);
}

/// A replay saved before its op gained fields still reads: each missing
/// field takes its type's zero (`None`, 0, false, an empty string, list or
/// struct).
#[test]
fn a_replay_reads_after_its_op_gains_fields() {
    #[derive(Clone, serde::Serialize)]
    enum Old {
        Tip { yocto: u64 },
    }
    #[derive(Debug, PartialEq, serde::Deserialize)]
    struct Note {
        text: String,
        tags: Vec<String>,
    }
    #[derive(Debug, PartialEq, serde::Deserialize)]
    enum New {
        Tip { yocto: u64, memo: Option<String>, times: u32, again: bool, note: Note },
    }
    let dir = temp_dir("fields");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("plan.json");
    Plan::new().op(Old::Tip { yocto: 5 }).save(&path);
    let plan = Plan::<New>::load(&path);
    let note = Note { text: String::new(), tags: vec![] };
    let tip = New::Tip { yocto: 5, memo: None, times: 0, again: false, note };
    assert_eq!(plan.ops(), [Some(&tip)]);
    let _ = std::fs::remove_dir_all(dir);
}

/// `w` writes the key it is given.
const WRITES: &str = r#"(module
  (import "env" "input" (func $input (param i64)))
  (import "env" "register_len" (func $len (param i64) (result i64)))
  (import "env" "read_register" (func $read (param i64 i64)))
  (import "env" "storage_write" (func $sw (param i64 i64 i64 i64 i64) (result i64)))
  (memory 1)
  (func (export "w")
    (call $input (i64.const 0))
    (call $read (i64.const 0) (i64.const 0))
    (drop (call $sw (call $len (i64.const 0)) (i64.const 0) (i64.const 1) (i64.const 0) (i64.const 1)))))"#;

/// A finding folded into a row whose case doesn't show it keeps its own
/// replay, named in the row's `also`: two checks broken by
/// the same kind of op, nothing failed, in two cases, are one row, and
/// each case is saved.
///
/// Verification boundary: the replay store over an actual scan.
#[test]
fn a_folded_finding_its_row_does_not_show_keeps_its_replay() {
    #[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
    enum Write {
        Key(String),
    }
    impl near_contract_sim::explore::Op for Write {
        fn submit(&self, sim: &mut Sim) -> Result<(), near_contract_sim::explore::Skip> {
            let Write::Key(key) = self;
            sim.call("alice.near", "c.near", "w").args(key.clone().into_bytes()).submit();
            Ok(())
        }
    }
    let dir = temp_dir("folded");
    let sim = Fixture::new()
        .account("alice.near", near_contract_sim::Balance::from_near(10))
        .contract("c.near", ::wat::parse_str(WRITES).unwrap())
        .build();
    let unset = |key: &'static str| {
        move |sim: &Sim| match sim.state("c.near", key.as_bytes()) {
            Some(_) => Err(format!("{key} was written")),
            None => Ok(()),
        }
    };
    let h = Harness::<Write>::new(sim)
        .store(&dir)
        .at_rest("no a", unset("a"))
        .at_rest("no b", unset("b"));
    let plans = [Plan::new().op(Write::Key("a".into())), Plan::new().op(Write::Key("b".into()))];
    let audit = h.scan(&plans).faults(Faults::none()).run();
    let [row] = audit.found() else { panic!("one row: {audit}") };
    let folded = row.also.iter().find(|a| a.starts_with("no b at world (")).expect("its replay");
    let files: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).collect();
    assert_eq!(files.len(), 2, "{files:?}");
    assert!(files.iter().any(|f| folded.contains(&f.display().to_string())), "{folded}");
    let replays = h.regressions();
    assert!(replays.iter().all(|r| matches!(r.status, Status::Shows(_))), "{replays:#?}");
    let _ = std::fs::remove_dir_all(dir);
}

/// `go` notes `pending` (the fixed build doesn't), calls `other.near`'s
/// `x`, then its own `cb`: `cb` clears `pending` when `x` succeeded and
/// panics when it failed, so a failed `x` leaves `pending` with no one to
/// clear it.
fn pending_wat(notes: bool) -> String {
    let note = if notes {
        "(drop (call $sw (i64.const 7) (i64.const 0) (i64.const 1) (i64.const 0) (i64.const 1)))"
    } else {
        ""
    };
    format!(
        r#"(module
  (import "env" "storage_write" (func $sw (param i64 i64 i64 i64 i64) (result i64)))
  (import "env" "storage_remove" (func $rm (param i64 i64 i64) (result i64)))
  (import "env" "current_account_id" (func $me (param i64)))
  (import "env" "register_len" (func $len (param i64) (result i64)))
  (import "env" "read_register" (func $read (param i64 i64)))
  (import "env" "promise_create"
    (func $create (param i64 i64 i64 i64 i64 i64 i64 i64) (result i64)))
  (import "env" "promise_then"
    (func $then (param i64 i64 i64 i64 i64 i64 i64 i64 i64) (result i64)))
  (import "env" "promise_result" (func $result (param i64 i64) (result i64)))
  (import "env" "panic" (func $panic))
  (memory 1)
  (data (i32.const 0) "pending")
  (data (i32.const 16) "x")
  (data (i32.const 32) "cb")
  (data (i32.const 48) "other.near")
  (func (export "go")
    (local $p i64)
    {note}
    (call $me (i64.const 0))
    (call $read (i64.const 0) (i64.const 1024))
    (local.set $p (call $create (i64.const 10) (i64.const 48) (i64.const 1) (i64.const 16)
                                (i64.const 0) (i64.const 0) (i64.const 64) (i64.const 5000000000000)))
    (drop (call $then (local.get $p) (call $len (i64.const 0)) (i64.const 1024) (i64.const 2)
                      (i64.const 32) (i64.const 0) (i64.const 0) (i64.const 64)
                      (i64.const 10000000000000))))
  (func (export "cb")
    (if (i64.ne (call $result (i64.const 0) (i64.const 1)) (i64.const 1)) (then (call $panic)))
    (drop (call $rm (i64.const 7) (i64.const 0) (i64.const 1))))
  (func (export "x")))"#
    )
}

/// The pending specimen: `c.near` on build `notes`, `other.near` with `x`;
/// `pending` left over is value owed to no one (it ranks first), and
/// `cb` must not fail.
fn pending(notes: bool) -> Harness<VaultOp> {
    let wasm = ::wat::parse_str(pending_wat(notes)).unwrap();
    let sim = Fixture::new()
        .account("alice.near", near_contract_sim::Balance::from_near(10))
        .contract("c.near", wasm.clone())
        .contract("other.near", wasm)
        .build();
    Harness::<VaultOp>::new(sim)
        .at_rest("pending, owed to no one", |sim: &Sim| match sim.state("c.near", b"pending") {
            Some(_) => Err("pending is left".to_owned()),
            None => Ok(()),
        })
        .must_not_fail("c.near::cb")
}

fn go() -> Plan<VaultOp> {
    Plan::new().call(near_contract_sim::explore::Call::new("alice.near", "c.near", "go"))
}

/// One forced fault breaks two checks: `x` failed, `cb`
/// panics (must not fail) and `pending` is left (the higher ranked, the
/// row). The row's own case shows both, so its one replay holds both
/// identities. On a build that fixes only `pending` (`cb` still panics)
/// the replay still shows: before, it held only the row's identity and
/// read `Gone` while `cb` still failed.
///
/// Verification boundary: replay judgment by the identities a replay
/// holds, over actual scans and replays on two builds.
#[test]
fn a_fold_its_row_shows_is_in_the_row_s_replay_and_keeps_it_showing() {
    let dir = temp_dir("forced-fold");
    let faults = Faults::new(Receipts::all(), [Fault::PromiseFailed]);
    let audit = pending(true).store(&dir).scan(&[go()]).faults(faults).run();
    let [row] = audit.found() else { panic!("one row: {audit}") };
    assert_eq!(row.finding.check, "pending, owed to no one", "{audit}");
    assert!(
        row.also.iter().any(|a| a.starts_with(near_contract_sim::explore::builtin::MUST_NOT_FAIL)),
        "{audit}"
    );
    // Fixed for `pending` only: `cb` still fails, so the replay shows.
    let half = pending(false);
    assert!(half.run(&go()).findings().is_empty(), "nothing fails unforced");
    let replays = half.clone().store(&dir).regressions();
    assert_eq!(replays.len(), 1, "{replays:#?}");
    assert_eq!(status(&replays[0]), "shows", "{:?}", replays[0]);
    // One file, holding both identities.
    let files: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).collect();
    assert_eq!(files, [row.replay.clone().unwrap()], "one file: {audit}");
    let json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&files[0]).unwrap()).unwrap();
    let also = json["also"].as_array().cloned().unwrap_or_default();
    assert!(
        also.iter().any(|id| id["check"] == near_contract_sim::explore::builtin::MUST_NOT_FAIL),
        "{json}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// Two harnesses share a store, at once: they find the same
/// finding, one with a check more. Each file either report cites exists
/// after both finish, and reads as its own harness's: before, both wrote
/// one file, the second's checks replaced the first's, and the harness
/// with fewer checks skipped its own replay as another's.
///
/// Verification boundary: the replay store over two actual scans run
/// concurrently.
#[test]
fn two_harnesses_share_a_store_at_once() {
    let dir = temp_dir("together");
    // Built in each thread: a harness isn't `Send`.
    let harness = |more: bool| {
        let h = vault_on("vault_forgetful.wasm");
        if more { h.at_rest("one more", |_: &Sim| Ok::<(), String>(())) } else { h }
    };
    let reports: Vec<_> = std::thread::scope(|s| {
        let runs: Vec<_> = [false, true]
            .map(|more| {
                let dir = dir.clone();
                s.spawn(move || {
                    let h = harness(more).store(&dir);
                    let audit =
                        h.scan(&[deposit_then_withdraw()]).faults(Faults::tolerable()).run();
                    let cited: Vec<PathBuf> = (audit.found().iter())
                        .flat_map(|x| x.replay.iter().cloned().chain(paths_in(&x.also)))
                        .collect();
                    (audit.to_string(), cited)
                })
            })
            .into_iter()
            .collect();
        runs.into_iter().map(|r| r.join().unwrap()).collect()
    });
    let harnesses = [harness(false), harness(true)];
    for (((audit, cited), h), more) in reports.iter().zip(&harnesses).zip([false, true]) {
        assert!(!cited.is_empty(), "{audit}");
        let mine = h.clone().store(&dir).regressions();
        for path in cited {
            assert!(path.exists(), "{} is gone:\n{audit}", path.display());
            let json: serde_json::Value =
                serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
            let checks = json["checks"].as_array().unwrap();
            assert_eq!(checks.contains(&"one more".into()), more, "another's checks: {json}");
            let r = mine.iter().find(|r| r.path == *path);
            let r = r.unwrap_or_else(|| panic!("{} isn't its harness's:\n{audit}", path.display()));
            assert_eq!(status(r), "shows", "{r:?}");
        }
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// The replay files a row's `also` names: `check at site (file)`.
fn paths_in(also: &[String]) -> Vec<PathBuf> {
    let file = |a: &String| Some(PathBuf::from(a.rsplit_once(" (")?.1.strip_suffix(')')?));
    also.iter().filter_map(file).collect()
}

/// A detached call to an account with no code: an unhandled failure.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct CallsMissing;

impl near_contract_sim::explore::Op for CallsMissing {
    fn submit(&self, sim: &mut Sim) -> Result<(), near_contract_sim::explore::Skip> {
        use fuzz_common::{Op as Fz, TGAS, encode};
        let call = Fz::Call {
            to: "missing.near".into(),
            args: encode(&[]),
            deposit: 0,
            gas: 10 * TGAS,
            weight: 0,
        };
        sim.call("alice.near", "contract.near", "run").args(encode(&[call])).submit();
        Ok(())
    }
}

/// A search that stops before it finishes leaves no replay; its saves
/// file carries its rows', and the search resumed and finished writes
/// them. Here the first search's replays are removed, as a
/// search killed after its last save and before its end leaves its
/// store, and the search resumed (nothing left to run) writes every row
/// its report cites: before, they were gone. Resumed on another store,
/// it writes them there.
///
/// Verification boundary: the replay store over an actual search saved
/// and resumed.
#[test]
fn a_resumed_search_writes_its_saved_rows_replays() {
    let dir = temp_dir("resumed");
    let saves = temp_dir("resumed-saves").join("saves.json");
    let sim: Sim = Fixture::new()
        .account("alice.near", near_contract_sim::Balance::from_near(10))
        .account("missing.near", near_contract_sim::Balance::from_near(1))
        .contract("contract.near", fuzz_common::fuzz_wasm())
        .build();
    let h = Harness::new(sim);
    let search = |dir: &PathBuf| {
        let strategy = plans(Just(CallsMissing)).len(1..=2).gas(gas::off());
        h.clone().store(dir).search(strategy).seed(4).cases(8).saves(&saves, 2).run()
    };
    let first = search(&dir);
    first.assert_found(builtin::UNHANDLED_FAILURE, "missing.near::run");
    std::fs::remove_dir_all(&dir).unwrap();
    let resumed = search(&dir).report();
    assert_eq!(resumed.cases, first.cases(), "nothing ran again");
    assert!(!resumed.findings.is_empty(), "{resumed}");
    for row in &resumed.findings {
        let path = row.replay.as_ref().unwrap_or_else(|| panic!("no replay: {resumed}"));
        assert!(std::path::Path::new(path).exists(), "{path} isn't there: {resumed}");
    }
    assert!(!resumed.json().contains("\"saved\""), "a finished report carries no replay");
    // A folded finding's own replay, carried with the row as a fold apart
    // would be: resumed elsewhere, its `also` cites the file written there.
    let mut carried: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&saves).unwrap()).unwrap();
    let row = &mut carried["findings"][0];
    let apart = dir.join("apart.json").display().to_string();
    let text = row["saved"][0][1].clone();
    row["saved"].as_array_mut().unwrap().push(serde_json::json!([apart, text]));
    row["also"].as_array_mut().unwrap().push(format!("other at world ({apart})").into());
    std::fs::write(&saves, carried.to_string()).unwrap();
    let other = temp_dir("resumed-elsewhere");
    let elsewhere = search(&other).report();
    for row in &elsewhere.findings {
        let path = PathBuf::from(row.replay.as_ref().expect("a replay"));
        assert!(path.starts_with(&other) && path.exists(), "{}: {elsewhere}", path.display());
    }
    let apart = paths_in(&elsewhere.findings[0].also);
    assert_eq!(apart, [other.join("apart.json")], "{elsewhere}");
    assert!(apart[0].exists(), "{elsewhere}");
    // Where the carried file can't be written (a directory holds its
    // name), `also` names no file, and the store is open.
    let blocked = temp_dir("resumed-blocked");
    std::fs::create_dir_all(blocked.join("apart.json")).unwrap();
    let there = search(&blocked).report();
    assert!(there.findings[0].also.contains(&"other at world".into()), "{there}");
    assert_eq!(paths_in(&there.findings[0].also), Vec::<PathBuf>::new(), "{there}");
    assert!(there.todo.iter().any(|t| t.id.starts_with("replays:")), "{there}");
    let _ = std::fs::remove_dir_all(blocked);
    let _ = std::fs::remove_dir_all(other);
    let _ = std::fs::remove_dir_all(dir);
    let _ = std::fs::remove_dir_all(saves.parent().unwrap());
}

/// A replay that can't be written is an open item, `replays:<dir>`, and
/// its row cites none: before, it reached only stderr.
/// Here the store's directory is a file.
#[test]
fn a_replay_that_cant_be_written_is_open() {
    let dir = temp_dir("unwritable");
    std::fs::write(&dir, "not a directory").unwrap();
    let audit = vault_on("vault_forgetful.wasm")
        .store(&dir)
        .scan(&[deposit_then_withdraw()])
        .faults(Faults::tolerable())
        .run();
    let found = audit.assert_found(PAYOUT, "Withdraw");
    assert!(found.replay.is_none(), "{audit}");
    let id = format!("replays:{}", dir.display());
    assert!(audit.todo().iter().any(|t| t.id == id), "{:#?}", audit.todo());
    assert!(audit.to_string().contains(&format!("TODO {id}")), "{audit}");
    let _ = std::fs::remove_file(dir);
}

/// `regressions()` without `Harness::store` reads the running test's
/// directory: missing, it panics naming it, since a test
/// that reads another test's replays reads nothing there. A store named
/// with `store(dir)`, missing or empty, has none: a clean campaign writes
/// nothing.
#[test]
fn regressions_without_a_store_name_the_directory_they_read() {
    // The test's own store, as an earlier run of it left it: none.
    let var = |k: &str| std::env::var_os(k).map(PathBuf::from);
    let manifest = common::manifest_dir();
    let root = var("NEAR_SIM_FOUND_DIR").unwrap_or_else(|| {
        let target =
            var("CARGO_TARGET_DIR").map_or_else(|| manifest.join("target"), |t| manifest.join(t));
        target.join("near-contract-sim/found")
    });
    let _ = std::fs::remove_dir_all(
        root.join("regressions_without_a_store_name_the_directory_they_read"),
    );
    let missing = std::panic::catch_unwind(|| vault_on("vault.wasm").regressions());
    let why = *missing.expect_err("panics").downcast::<String>().unwrap();
    assert!(why.contains("regressions_without_a_store_name_the_directory_they_read"), "{why}");
    assert!(why.contains(".store(dir)"), "{why}");
    // A campaign of the test, clean, makes it: none, no panic.
    let clean =
        vault_on("vault.wasm").scan(&[deposit_then_withdraw()]).faults(Faults::none()).run();
    assert!(clean.found().is_empty(), "{clean}");
    assert!(vault_on("vault.wasm").regressions().is_empty());
    let dir = temp_dir("empty");
    assert!(vault_on("vault.wasm").store(&dir).regressions().is_empty());
    std::fs::create_dir_all(&dir).unwrap();
    assert!(vault_on("vault.wasm").store(&dir).regressions().is_empty());
    let _ = std::fs::remove_dir_all(dir);
}

/// `regressions()` says on stderr what it read: this harness's replays,
/// other harnesses' and unreadable files, so a truncated
/// replay isn't dropped unseen.
#[test]
fn regressions_count_what_they_skip() {
    let dir = temp_dir("counted");
    forgetful_replay(&dir);
    std::fs::write(dir.join("truncated.json"), "{\"check\": \"with").unwrap();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "counted_child", "--include-ignored", "--nocapture"])
        .env("NCS_REPLAY_DIR", &dir)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "child failed:\n{stderr}");
    let line = format!("replays of this harness in {}", dir.display());
    let line = stderr.lines().find(|l| l.contains(&line)).unwrap_or_else(|| panic!("{stderr}"));
    assert!(line.contains("1 unreadable"), "{line}");
    let _ = std::fs::remove_dir_all(dir);
}

/// Runs only from `regressions_count_what_they_skip`.
#[test]
#[ignore = "child process of regressions_count_what_they_skip"]
fn counted_child() {
    let Ok(dir) = std::env::var("NCS_REPLAY_DIR") else { return };
    assert!(!vault_on("vault_forgetful.wasm").store(dir).regressions().is_empty());
}
