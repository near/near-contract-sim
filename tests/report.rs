//! The audit's report (`Display`, `Report::json`, `todo`, `done`) on the
//! intents-like custody (`tests/contracts/custody`): findings ranked by
//! consequence, which exported methods ran, and a to-do list with stable ids
//! that excuses close, visibly.
//!
//! Verification boundary: the report's reading of an audit (ranking,
//! items, ids, JSON), over actual simulator runs. No schedule changes.

mod common;
use common::res;
use near_contract_sim::explore::builtin;
use near_contract_sim::explore::{Audit, Books, Call, Harness, Op, Plan, Report, Skip};
use near_contract_sim::{Balance, Fixture, Reply, Sim, Stub};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeMap;

const TOKEN: &str = "token.near";
const CUSTODY: &str = "custody.near";

fn harness(wasm: &str) -> Harness<Deposit> {
    harness_of(wasm)
}

fn harness_of<O: Op>(wasm: &str) -> Harness<O> {
    let f = Fixture::new()
        .account("alice.near", Balance::from_near(10))
        .contract(TOKEN, res("fungible_token.wasm"))
        .init("new_default_meta")
        .args_json(json!({ "owner_id": TOKEN, "total_supply": "10000000" }))
        .contract(CUSTODY, res(wasm))
        .init("new")
        .args_json(json!({ "token_id": TOKEN }))
        .stub("sink.near", Stub::new().on("on_notify", Reply::json("refund all")))
        .ft_register(TOKEN, [CUSTODY, "sink.near", "alice.near"])
        .ft_transfer(TOKEN, TOKEN, "alice.near", 1_000_000);
    let view = |sim: &Sim, receiver: &str, method: &str, args| -> u128 {
        sim.view(receiver, method, args).u128()
    };
    let books = Books::new()
        .liability("credit", move |s: &Sim| {
            BTreeMap::from([("all".into(), view(s, CUSTODY, "total_credit", json!({})))])
        })
        .asset("tokens", move |s: &Sim| {
            view(s, TOKEN, "ft_balance_of", json!({ "account_id": CUSTODY }))
        })
        .backs("credit", "tokens");
    Harness::new(f.build()).must_not_fail("resolve_deposit").books(books).audits([CUSTODY])
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Deposit {
    amount: u16,
    ids: Vec<String>,
    memo: String,
}

impl Op for Deposit {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        let msg =
            json!({ "receiver": "sink.near", "ids": self.ids, "memo": self.memo, "notify": true });
        sim.call("alice.near", TOKEN, "ft_transfer_call")
            .args_json(json!({
                "receiver_id": CUSTODY,
                "amount": self.amount.to_string(),
                "msg": msg.to_string(),
            }))
            .deposit(Balance::from_yoctonear(1))
            .submit();
        Ok(())
    }
}

fn witness() -> Plan<Deposit> {
    Plan::new().op(Deposit { amount: 100, ids: vec!["a".into()], memo: "m".into() })
}

fn curves(h: &Harness<Deposit>) -> Audit<Deposit> {
    h.curves(&[witness()]).run()
}

/// The resolver's crossing leaves the credit unbacked: the books finding
/// ranks first, above the failed receipt; the summary fits 60 lines.
#[test]
fn findings_rank_by_consequence_in_a_short_summary() {
    let h = harness("custody_gas_bug.wasm");
    let audit = curves(&h);
    let summary = audit.to_string();
    eprintln!("{summary}");
    assert!(summary.lines().count() <= 60, "{} lines", summary.lines().count());
    let r = audit.report();
    assert_eq!(r.findings[0].kind, "books", "{summary}");
    // The resolver's failure is one cause: its failed receipt is listed
    // under the books row, with its own replay, not a row of its own.
    let builtins = [builtin::UNHANDLED_FAILURE, builtin::FAILED_CALLBACK, builtin::MUST_NOT_FAIL];
    let failed = |a: &String| builtins.iter().any(|c| a.starts_with(&format!("{c} at ")));
    assert!(r.findings[0].also.iter().any(failed), "{summary}");
    assert!(r.findings[0].steps[0].contains("ids: ["), "{summary}");
    let head = format!("{} finding(s), ", r.findings.len());
    assert!(summary.starts_with(&head), "the headline counts the findings: {summary}");
    assert!(summary.contains(&format!("; {} item(s) open.", r.todo.len())), "{summary}");
    assert_eq!(audit.to_string(), summary, "Display is the summary");
    assert_eq!(audit.report().to_string(), summary, "and the report's");
    assert!(summary.contains("coverage: "), "{summary}");
}

/// A summary names curves only when curves ran: a scan's has no curve
/// guarantee and no curve-row count; curves' has both.
#[test]
fn a_summary_without_curves_never_names_them() {
    let h = harness("custody_gas_bug.wasm");
    let scan = h.scan(&[witness()]).run().to_string();
    assert!(scan.contains("Only what was tried.") && !scan.contains("curve row(s)"), "{scan}");
    assert!(!scan.contains("crossed curve"), "{scan}");
    let curves = curves(&h).to_string();
    assert!(curves.contains("crossed curve") && curves.contains("curve row(s)"), "{curves}");
}

/// The same harness and plans give the same items, by id; the report
/// reads back from its JSON.
#[test]
fn todo_ids_are_stable_and_the_report_round_trips() {
    let h = harness("custody.wasm");
    let (a, b) = (curves(&h), curves(&h));
    let ids = |x: &Audit<Deposit>| x.todo().into_iter().map(|t| t.id).collect::<Vec<_>>();
    assert!(!ids(&a).is_empty());
    assert_eq!(ids(&a), ids(&b));
    let loaded = Report::from_json(&a.report().json()).unwrap();
    assert_eq!(loaded, a.report());
    assert_eq!(loaded.to_string(), a.to_string(), "a saved report prints as its audit");
    // Timings are f64 seconds: one serde_json's default parser reads back
    // an ulp off (seen in curves.rs) still round-trips.
    let mut timed = a.report();
    timed.timings = vec![("t".into(), "1.4203129159999999".parse().unwrap())];
    let json = serde_json::to_string(&timed).unwrap();
    assert_eq!(Report::from_json(&json).unwrap(), timed);
    // The custody's methods no case ran are left open.
    assert!(ids(&a).iter().any(|i| i == "unreached:custody.near::withdraw"), "{:?}", ids(&a));
    // Its JSON carries this build's format version. One written before
    // versions (0), or in an older format, is refused: its rows'
    // identities may read otherwise now, and a union would hold one
    // finding twice. One written by a newer format is refused, not
    // misread. A union refuses either.
    let format = a.report().format;
    let mut json: serde_json::Value = serde_json::from_str(&a.report().json()).unwrap();
    assert_eq!(json["format"], format, "{json}");
    json.as_object_mut().unwrap().remove("format");
    let unversioned = Report::from_json(&json.to_string()).unwrap_err();
    assert!(unversioned.contains("written in format 0, an older identity scheme"), "{unversioned}");
    json["format"] = (format - 1).into();
    let older = Report::from_json(&json.to_string()).unwrap_err();
    assert!(older.contains("older identity scheme than this build's"), "{older}");
    json["format"] = (format + 1).into();
    let newer = Report::from_json(&json.to_string()).unwrap_err();
    assert!(newer.contains(&format!("newer than this build's {format}")), "{newer}");
    let mut old = a.report();
    old.format = format - 1;
    let union = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| h.union(&[old])));
    assert!(union.is_err(), "a union of an older report");
}

/// An excuse closes items by id or glob, and the header says so; the
/// audit is done only when nothing is open.
#[test]
fn excuses_close_items_visibly() {
    let h = harness("custody.wasm");
    let open = curves(&h).todo();
    assert!(curves(&h).done().is_err());
    let mut excused = h.excuse("unreached:custody.near::*", "the deposit path only, here");
    for t in open.iter().filter(|t| !t.id.starts_with("unreached:")) {
        excused = excused.excuse(&t.id, "accepted for this test");
    }
    let audit = curves(&excused);
    let summary = audit.done().unwrap_or_else(|t| panic!("still open: {t:#?}"));
    assert!(summary.contains("excused unreached:custody.near::* ("), "{summary}");
    assert!(summary.contains("left out of the audit: "), "{summary}");
}

/// The honest override: the write-up goes ahead with every open item
/// first, under why.
#[test]
fn open_gaps_print_first() {
    let audit = curves(&harness("custody.wasm"));
    let text = audit.with_open_gaps("a first pass");
    let first = text.lines().next().unwrap();
    assert!(first.starts_with("Coverage not achieved (a first pass): "), "{text}");
    let item = &audit.todo()[0].id;
    assert!(text.find(item.as_str()) < text.find("Only what was tried"), "{text}");
}

/// A replay whose check the harness no longer has is a to-do item, found
/// when the audit saves, without running it. The store reads each file
/// once per process; a file deleted or added later is seen at the next
/// save.
#[test]
fn a_stale_replay_is_open() {
    let dir = std::env::temp_dir().join(format!("ncs-report-stale-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let h = harness("custody.wasm").store(&dir);
    // Of this build's format: an older one is stale for that.
    let format = h.union(&[]).format;
    let old = json!({ "format": format, "check": "gone", "site": "x", "label": "shown", "fixture": "", "plan": {} });
    std::fs::write(dir.join("gone@x-0.json"), old.to_string()).unwrap();
    let stale = |a: &Audit<Deposit>| {
        a.todo().into_iter().filter(|t| t.id.starts_with("stale:")).collect::<Vec<_>>()
    };
    let first = stale(&curves(&h));
    assert_eq!(first.len(), 1, "{first:?}");
    assert_eq!(first[0].id, format!("stale:{}", dir.join("gone@x-0.json").display()));
    assert!(first[0].why.contains("no check \"gone\""));
    std::fs::remove_file(dir.join("gone@x-0.json")).unwrap();
    assert_eq!(stale(&curves(&h)), [], "a deleted replay is no longer open");
    std::fs::write(dir.join("gone@y-1.json"), old.to_string()).unwrap();
    let again = stale(&curves(&h));
    let id = format!("stale:{}", dir.join("gone@y-1.json").display());
    assert_eq!(again.iter().map(|t| t.id.clone()).collect::<Vec<_>>(), [id]);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A method seen to run as a view (the books' `total_credit`) ran, so it
/// is not unreached.
#[test]
fn a_view_a_check_reads_is_not_open() {
    let r = curves(&harness("custody.wasm")).report();
    assert!(!r.paths.unreached.iter().any(|s| s == "custody.near::total_credit"), "{:?}", r.paths);
}

/// Coverage is the union's: a method only another audit reached is open
/// in neither `Curves::with` nor `Harness::union` of the saved reports;
/// items other than coverage stay open.
#[test]
fn a_method_another_audit_reached_is_not_open() {
    let h = harness("custody.wasm");
    let withdraw =
        Call::new("alice.near", CUSTODY, "withdraw").args_json(json!({ "amount": "10" }));
    let other = h.curves(&[witness().call(withdraw)]).run();
    let id = "unreached:custody.near::withdraw";
    let open = |todo: Vec<near_contract_sim::explore::Todo>| todo.iter().any(|t| t.id == id);
    let alone = curves(&h);
    assert!(open(alone.todo()));
    assert!(!open(other.todo()), "{:#?}", other.todo());
    let with = h.curves(&[witness()]).with(&other).run();
    assert!(!open(with.todo()), "{:#?}", with.todo());
    let union = h.union(&[alone.report(), other.report()]);
    assert!(!open(union.todo.clone()), "{:#?}", union.todo);
    assert!(!union.paths.unreached.iter().any(|s| s == "custody.near::withdraw"));
    let kept = alone.todo().into_iter().find(|t| !t.id.starts_with("unreached:"));
    if let Some(t) = kept {
        assert!(union.todo.iter().any(|x| x.id == t.id), "{t:?} stays open");
    }
}

/// Coverage is what ran: the near-sdk desk's three views
/// (`balance_of`, `pending_of`, `total`), which no case, setup or check
/// calls, are open `unreached:` items like any method, until a plan or a
/// check calls them or an excuse closes them; nothing calls them to guess.
/// The summary prints them as one line, and one glob closes them all.
///
/// Verification boundary: the report's reading of coverage, over actual
/// runs on the simulator.
#[test]
fn a_view_nothing_calls_is_open() {
    const DESK: &str = "desk.near";
    let world = || {
        Fixture::new()
            .account("alice.near", Balance::from_near(100))
            .stub("issuer.near", Stub::new().on("issue", Reply::json("ok")))
            .contract(DESK, res("desk.wasm"))
            .init("new")
            .args_json(json!({ "issuer": "issuer.near" }))
            .call("alice.near", DESK, "deposit")
            .deposit(Balance::from_near(50))
            .build()
    };
    let call = |method: &str, args| Call::new("alice.near", DESK, method).args_json(args);
    let redeem = json!({ "tickets": ["00000001"], "amount": "1000" });
    let plans =
        [Plan::new().op(call("register", json!({}))), Plan::new().op(call("redeem", redeem))];
    let h = Harness::new(world()).audits([DESK]);
    let audit = h.scan(&plans).faults(near_contract_sim::explore::Faults::none()).run();
    let open: Vec<String> = audit.todo().into_iter().map(|t| t.id).collect();
    for view in ["balance_of", "pending_of", "total"] {
        assert!(open.contains(&format!("unreached:{DESK}::{view}")), "{view}: {audit}");
    }
    // Kept per method, printed as one line, in the summary and the gaps.
    let n = open.iter().filter(|id| id.starts_with("unreached:")).count();
    let head = format!("unreached: {n} exported methods never ran: ");
    // The gaps' list, then the summary: one line in each.
    for (text, k) in [(audit.to_string(), 1), (audit.with_open_gaps("a first pass"), 2)] {
        let lines: Vec<&str> = text.lines().filter(|l| l.contains("unreached:")).collect();
        assert_eq!(lines.len(), k, "{text}");
        for line in lines {
            assert!(line.trim_start().starts_with(&head), "{text}");
            assert!(line.contains(&format!("{DESK}::balance_of")), "{text}");
        }
    }
    // One glob closes the family, each method listed under it.
    let excused = Harness::new(world()).audits([DESK]).excuse("unreached:desk.near::*", "views");
    let audit = excused.scan(&plans).faults(near_contract_sim::explore::Faults::none()).run();
    assert!(audit.todo().iter().all(|t| !t.id.starts_with("unreached:")), "{audit}");
    let report = audit.report();
    let closed = &report.excused.iter().find(|e| e.0 == "unreached:desk.near::*").unwrap().2;
    assert_eq!(closed.len(), n, "{report:#?}");
    let summary = audit.to_string();
    assert!(
        summary.contains(&format!("excused unreached:desk.near::* ({n} item(s))")),
        "{summary}"
    );
    assert!(!summary.contains("never ran:"), "{summary}");
}

/// A union's coverage is the members' exercised paths, never their
/// excuses: with every unreached path excused, each member and the union
/// print the same `coverage: N of M`, N < M, and the union keeps the
/// excuses apart (counted as exercised, they would read 73 of 73 over
/// members' 33 of 73).
///
/// Verification boundary: the report's union of actual scans' coverage.
#[test]
fn a_union_counts_excused_paths_as_unreached() {
    const DESK: &str = "desk.near";
    let world = Fixture::new()
        .account("alice.near", Balance::from_near(100))
        .stub("issuer.near", Stub::new().on("issue", Reply::json("ok")))
        .contract(DESK, res("desk.wasm"))
        .init("new")
        .args_json(json!({ "issuer": "issuer.near" }))
        .build();
    let register = Plan::new().op(Call::new("alice.near", DESK, "register").args_json(json!({})));
    let h = Harness::new(world).audits([DESK]).excuse("unreached:*", "a later pass");
    let scan = || {
        h.scan(std::slice::from_ref(&register))
            .faults(near_contract_sim::explore::Faults::none())
            .run()
            .report()
    };
    let (a, b) = (scan(), scan());
    let line =
        |r: &Report| r.to_string().lines().find(|l| l.starts_with("coverage:")).unwrap().to_owned();
    assert!(!a.paths.unreached.is_empty(), "{a}");
    assert!(a.todo.iter().all(|t| !t.id.starts_with("unreached:")), "{a}");
    let u = h.union(&[a.clone(), b]);
    assert_eq!(line(&u), line(&a), "{u}");
    assert_eq!(u.paths.unreached, a.paths.unreached);
    assert!(u.excused.iter().any(|e| e.0 == "unreached:*" && !e.2.is_empty()), "{u}");
}

/// While set, a [`Toggled`] deposit of an odd amount panics in `submit`.
static PANICS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// A deposit whose op panics on odd amounts while [`PANICS`] is set: an
/// op fixed between two builds of the test.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Toggled {
    amount: u16,
}

impl Op for Toggled {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        let panics = PANICS.load(std::sync::atomic::Ordering::SeqCst);
        assert!(!panics || self.amount.is_multiple_of(2), "odd amounts are broken");
        Deposit { amount: self.amount, ids: vec!["a".into()], memo: "m".into() }.submit(sim)
    }
}

/// `Harness::union` of saved reports:
/// - an excused unreached method is still unreached, not exercised;
/// - each report's excuses close its items in the union too;
/// - a resumed campaign's report, which holds the saved one, counts its
///   runs once, and its seeds once;
/// - a stale replay since deleted is no item.
///
/// Verification boundary: the report's union of saved reports, over
/// actual searches and curves, saved, resumed and replayed.
#[test]
fn union_counts_each_run_once_and_keeps_every_excuse() {
    use near_contract_sim::explore::plans;
    use proptest::prelude::*;
    let dir = std::env::temp_dir().join(format!("ncs-report-union-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let ids = |r: &Report| r.todo.iter().map(|t| t.id.clone()).collect::<Vec<_>>();
    let withdraw = "unreached:custody.near::withdraw";
    // Excused in its own report, unreached in the union, closed by its
    // report's excuse though this harness has none.
    let excused = harness("custody.wasm").excuse(withdraw, "a later pass").store(&dir);
    let stale =
        json!({ "check": "gone", "site": "x", "label": "shown", "fixture": "", "plan": {} });
    std::fs::write(dir.join("gone@x-0.json"), stale.to_string()).unwrap();
    let curves = excused.curves(&[witness()]).run().report();
    let stale = format!("stale:{}", dir.join("gone@x-0.json").display());
    assert!(ids(&curves).contains(&stale), "{:?}", ids(&curves));
    std::fs::remove_file(dir.join("gone@x-0.json")).unwrap();
    let h = harness("custody.wasm");
    let u = h.union(std::slice::from_ref(&curves));
    // Never ran: open all the same, if excused.
    assert!(u.paths.unreached.iter().any(|s| s == "custody.near::withdraw"), "{:?}", u.paths);
    assert!(!ids(&u).iter().any(|id| id == withdraw || id == &stale), "{:?}", ids(&u));
    assert!(u.excused.iter().any(|e| e.0 == withdraw && e.2 == [withdraw]), "{:?}", u.excused);
    // A search whose op panics, saved, and resumed by the same build; a
    // later build fixes the op and starts over, clean.
    let toggled = harness_of::<Toggled>("custody.wasm");
    let path = dir.join("search.json");
    let search = || {
        let strategy = plans((1u16..100).prop_map(|amount| Toggled { amount })).len(1..=1);
        toggled.search(strategy).cases(6).seed(7).saves(&path, 1).run().report()
    };
    let broken = search();
    assert!(ids(&broken).contains(&"errors".to_owned()), "{:?}", ids(&broken));
    let same = search();
    assert!(ids(&same).contains(&"errors".to_owned()), "the same build judges alike");
    let mut saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    saved["build"] = json!("another");
    std::fs::write(&path, saved.to_string()).unwrap();
    PANICS.store(false, std::sync::atomic::Ordering::SeqCst);
    let fixed = search();
    assert!(!ids(&fixed).contains(&"errors".to_owned()), "{:?}", ids(&fixed));
    assert_eq!(fixed.cases, 6, "{fixed:?}");
    let u = toggled.union(&[broken.clone(), same]);
    assert_eq!((u.cases, u.seeds.clone()), (6, vec![7]), "each run once");
    // Two separate runs of one seed: their cases add up, the seed once.
    std::fs::remove_file(&path).unwrap();
    let again = search();
    let u = toggled.union(&[fixed, again]);
    assert_eq!((u.cases, u.seeds.clone()), (12, vec![7]));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A check judged in some cases and not in others (its op overlapped
/// another) is counted, not hidden: the summary names how
/// often it wasn't judged and why, and never says "nothing open" beside
/// it; a union keeps the count. It is a summary line, not an open item:
/// a check judged once is covered, as a method reached once is.
///
/// Verification boundary: the report's reading of an audit's judgements,
/// over actual scans of overlapping ops. No schedule changes.
#[test]
fn a_check_not_judged_in_some_cases_is_counted() {
    use near_contract_sim::explore::{Fail, Faults, OpDone};
    const REGISTRY: &str = "registry.near";
    const CHECK: &str = "register judged alone";
    let text = std::fs::read_to_string(common::manifest_dir().join("tests/res/open_registry.wat"));
    let world = Fixture::new()
        .account("alice.near", Balance::from_near(100))
        .contract(REGISTRY, ::wat::parse_str(text.unwrap()).unwrap())
        .build();
    // It never asks isolated(): reading the shared world is enough.
    let h = Harness::new(world).per_op(CHECK, |d: &OpDone<Call>| {
        let _ = d.world();
        Ok::<_, Fail>(())
    });
    let register = || Call::new("alice.near", REGISTRY, "register");
    let alone = Plan::new().op(register());
    let together = Plan::new().op(register()).blocks(0).op(register());
    let scan = || h.scan(&[alone.clone(), together.clone()]).faults(Faults::none()).run();
    let audit = scan();
    let shown = audit.to_string();
    assert!(!shown.contains("nothing open"), "{shown}");
    let line = shown.lines().find(|l| l.starts_with("not judged:"));
    let line = line.unwrap_or_else(|| panic!("no not-judged line:\n{shown}"));
    assert!(line.contains(CHECK) && line.contains("overlapped: it read"), "{line}");
    assert!(audit.todo().iter().all(|t| t.id != format!("unjudged:{CHECK}")), "{shown}");
    let n = |r: &Report| {
        let line = r.to_string().lines().find(|l| l.starts_with("not judged:")).map(String::from);
        line.unwrap_or_default()
    };
    let one = audit.report();
    let u = h.union(&[one.clone(), scan().report()]);
    let count = |line: &str| line.split(CHECK).nth(1).unwrap_or_default().to_owned();
    let (once, twice) = (count(&n(&one)), count(&n(&u)));
    let first = |s: &str| -> u32 {
        let digits: String = s.trim().chars().take_while(char::is_ascii_digit).collect();
        digits.parse().unwrap_or(0)
    };
    assert!(first(&once) > 0 && first(&twice) == 2 * first(&once), "{one}\n{u}");
}
