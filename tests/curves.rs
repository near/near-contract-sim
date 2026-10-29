//! Curves (`Harness::curves`, `Harness::curve`) on the specimens whose
//! shapes are known: what each receipt uses as an axis of a witness
//! moves, against its limit.
//!
//! Verification boundary: the ladders' runs (which plans run, with
//! copies settled before a step or a tail during a hold) and the rows'
//! judgment, against actual simulator runs. The rows are observations
//! of bounded ladders: a row below its limit proves nothing past where
//! its axis moved.

mod common;
use common::res;
use near_contract_sim::explore::{Call, Harness, Op, Plan, Reach, Report, Resource, Skip, Status};
use near_contract_sim::{Balance, Fixture, Reply, Sim, Stub};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// The drivers specimen's `funded` and `capped` builds: `pledge(holder,
/// ids, amounts)` refuses when the amounts' sum passes the caller's limit
/// of 100 ("insufficient funds"), never on the count; `capped` also
/// refuses 32 ids or more ("batch too long"). `on_pledged` records them
/// with a budget of 5 + 10 n Tgas, so past 29 ids the wallet's 300 Tgas
/// can't carry the pledge: the ladder runs at the ceiling's gas.
mod drivers {
    use super::*;

    const HOLDINGS: &str = "holdings.near";
    const ALICE: &str = "alice.near";
    const ON_PLEDGED: &str = "holdings.near::on_pledged";

    fn harness(wasm: &str) -> Harness<Pledge> {
        let f = Fixture::new()
            .account(ALICE, Balance::from_near(1000))
            .stub("ledger.near", Stub::new().on("record", Reply::json("ok")))
            .contract(HOLDINGS, res(wasm))
            .init("new")
            .args_json(json!({ "ledger": "ledger.near" }))
            .call(ALICE, HOLDINGS, "fund")
            .args_json(json!({ "account": ALICE, "amount": "100" }))
            .call(ALICE, HOLDINGS, "issue")
            .args_json(json!({ "ids": ["t0", "t1"] }));
        Harness::new(f.build()).must_not_fail("on_pledged")
    }

    /// A pledge of ids by number (`t<n>`), an amount each.
    #[derive(Clone, Debug, Serialize, Deserialize)]
    struct Pledge {
        ids: Vec<u16>,
        amounts: Vec<u8>,
    }

    impl Op for Pledge {
        fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
            let ids: Vec<String> = self.ids.iter().map(|n| format!("t{n}")).collect();
            let amounts: Vec<String> = self.amounts.iter().map(u8::to_string).collect();
            sim.call(ALICE, HOLDINGS, "pledge")
                .args_json(json!({ "holder": ALICE, "ids": ids, "amounts": amounts }))
                .submit();
            Ok(())
        }
    }

    fn witness(amount: u8) -> Plan<Pledge> {
        Plan::new().op(Pledge { ids: vec![0, 1], amounts: vec![amount, amount] })
    }

    /// The capped build refuses 32 ids, and still does with the amounts at
    /// 1, while 31 reach `on_pledged`: a cap, with its evidence. The ids
    /// grew by copies of the last: a distinct id's cost stays to do.
    #[test]
    fn a_refusal_on_the_count_is_an_entry_cap() {
        let h = harness("drivers_capped.wasm");
        let audit = h.curve(&witness(1), 0, "ids").run();
        eprintln!("{audit}");
        let row = audit.assert_curve(ON_PLEDGED, Resource::ReceiptGas, "ids");
        let Reach::EntryCap { cap, evidence } = &row.reach else { panic!("{row}") };
        assert_eq!(*cap, 32, "{row}");
        assert!(evidence.contains("31 passes") && evidence.contains("batch too long"), "{row}");
        assert_eq!(row.open(), None, "{row}");
        // The row prints the plan it measured last, its real steps.
        let ids = format!("ids: [0, {}]", vec!["1"; 30].join(", "));
        assert!(row.to_string().contains(&format!("step 0: Op(Pledge {{ {ids}")), "{row}");
        // Numbers take no fresh value: the longer list repeats `t1`, and
        // a cost per distinct id is an open item, not a pass.
        let todo = audit.todo();
        let item = todo.iter().find(|t| t.id == "distinct:/ids");
        let item = item.unwrap_or_else(|| panic!("{todo:#?}"));
        assert!(item.why.contains("grows only by copies of one item"), "{item:?}");
    }

    /// The funded build refuses 3 ids of 40 on the amounts' sum; with the
    /// amounts at 1 it passes, so it is no cap. The refusal changed
    /// nothing: a bound of the world (alice's balance), the row closed
    /// with it.
    #[test]
    fn a_refusal_on_the_amounts_is_no_cap() {
        let h = harness("drivers_funded.wasm");
        let audit = h.curve(&witness(40), 0, "ids").run();
        eprintln!("{audit}");
        let row = audit.assert_curve(ON_PLEDGED, Resource::ReceiptGas, "ids");
        let Reach::AtBound(why) = &row.reach else { panic!("{row}") };
        assert!(why.contains("insufficient funds") && why.starts_with("3 fails"), "{row}");
        assert!(row.to_string().contains("stopped at a bound: 3 fails at `holdings.near"), "{row}");
        assert_eq!(row.open(), None, "{row}");
    }
}

/// The keeper specimen (see `tests/keeper.rs`): a deposit through the
/// token, and with the `ping` builds a `ping(account, ids)` whose
/// `on_ping` hashes per id and touches no key.
mod keeper {
    use super::*;

    const KEEPER: &str = "keeper.near";
    const TOKEN: &str = "mtoken.near";
    const ALICE: &str = "alice.near";
    const RECEIVER: &str = "r0.near";
    const ON_RESOLVE: &str = "keeper.near::on_resolve";

    /// The token made with ids `id0`.. (`gold` for one), all Alice's.
    fn harness(keeper: &str, ids: usize) -> Harness<KOp> {
        let refund_all = Reply::json(vec![u128::MAX.to_string(); 64]);
        let ids: Vec<String> = match ids {
            1 => vec!["gold".into()],
            n => (0..n).map(|i| format!("id{i}")).collect(),
        };
        let mut f = Fixture::new()
            .account("issuer.near", Balance::from_near(1000))
            .account(ALICE, Balance::from_near(1000))
            .stub(RECEIVER, Stub::new().on("on_transfer", refund_all))
            .contract(TOKEN, res("mtoken.wasm"))
            .init("new")
            .args_json(json!({ "owner": "issuer.near", "ids": ids }))
            .contract(KEEPER, res(keeper))
            .init("new")
            .args_json(json!({ "token": TOKEN }));
        for id in &ids {
            let args = json!({ "to": ALICE, "id": id, "amount": "1000000" });
            f = f.call("issuer.near", TOKEN, "give").args_json(args);
        }
        Harness::new(f.build()).must_not_fail("on_resolve")
    }

    #[derive(Clone, Debug, Serialize, Deserialize)]
    enum KOp {
        /// `ids` through the token to the receiver, which is told.
        Deposit { ids: Vec<String> },
        /// `ping(receiver, ids)`.
        Ping { ids: Vec<String> },
    }

    impl Op for KOp {
        fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
            match self {
                KOp::Deposit { ids } => sim
                    .call(ALICE, TOKEN, "batch_transfer_call")
                    .args_json(json!({
                        "receiver_id": KEEPER,
                        "token_ids": ids,
                        "amounts": vec!["1"; ids.len()],
                        "msg": json!({ "receiver": RECEIVER, "notify": "hi" }).to_string(),
                    }))
                    .submit(),
                KOp::Ping { ids } => sim
                    .call(ALICE, KEEPER, "ping")
                    .args_json(json!({ "account": RECEIVER, "ids": ids }))
                    .submit(),
            };
            Ok(())
        }
    }

    /// A tail of direct `on_transfer`s during the held `on_resolve` of a
    /// one-id deposit: the resolver moves one entry whatever the tail, so
    /// its gas plateaus and no crossing is claimed. (With 16 credited ids
    /// it crosses: see
    /// `a_tail_during_the_hold_crosses_from_the_trigger_alone`.)
    #[test]
    fn a_tail_during_the_hold_plateaus_on_one_id() {
        let h = harness("keeper.wasm", 1);
        let witness = Plan::new().op(KOp::Deposit { ids: vec!["gold".into()] });
        let tail = "keeper.near::on_transfer while keeper.near::on_resolve is held";
        let audit = h.curve(&witness, 0, tail).run();
        eprintln!("{audit}");
        let row = audit.assert_curve(ON_RESOLVE, Resource::ReceiptGas, tail);
        assert_eq!(row.reach, Reach::Below, "{row}");
        assert_eq!(row.moved_to, 64, "{row}");
    }

    /// A per-op check that reads the shared world judges nothing while a
    /// tail overlaps the op, and that alone doesn't stop the tail: it
    /// climbs as it does without the check, and the check
    /// stays an open `incomplete:` item named after it.
    ///
    /// Verification boundary: when a curve's ladder stops, and the to-do
    /// list, over actual runs.
    #[test]
    fn an_overlapped_per_op_check_does_not_stop_a_tail() {
        let h = harness("keeper.wasm", 1).per_op("reads the world", |d| {
            let _ = d.world();
            Ok::<(), String>(())
        });
        let witness = Plan::new().op(KOp::Deposit { ids: vec!["gold".into()] });
        let tail = "keeper.near::on_transfer while keeper.near::on_resolve is held";
        let audit = h.curve(&witness, 0, tail).run();
        let row = audit.assert_curve(ON_RESOLVE, Resource::ReceiptGas, tail);
        assert_eq!((row.moved_to, &row.reach), (64, &Reach::Below), "{row}");
        let id = "incomplete:reads the world not judged (overlapped)";
        assert!(audit.todo().iter().any(|t| t.id == id), "{:#?}", audit.todo());
    }

    /// A tail whose case stops being judged past some copies is open, as
    /// a repeat's is: never an entry cap, which a control with
    /// the numbers at 1 can't tell apart for copies. Here a check can't
    /// judge a world of more than one copy's transactions.
    ///
    /// Verification boundary: how a curve judges its stop, over actual
    /// runs.
    #[test]
    fn a_tail_stopped_by_its_case_is_open_not_a_cap() {
        let witness = Plan::new().op(KOp::Deposit { ids: vec!["gold".into()] });
        let h = harness("keeper.wasm", 1);
        let ran = h.run(&witness).world::<Sim>().unwrap().txs().len();
        let h = h.at_rest("judges a few transactions", move |sim: &Sim| match sim.txs().len() {
            n if n > ran + 1 => Err(near_contract_sim::explore::Fail::not_judged("too many")),
            _ => Ok(()),
        });
        let tail = "keeper.near::on_transfer while keeper.near::on_resolve is held";
        let audit = h.curve(&witness, 0, tail).run();
        let row = audit.assert_curve(ON_RESOLVE, Resource::ReceiptGas, tail);
        let Reach::Stopped(why) = &row.reach else { panic!("{row}") };
        assert!(why.contains("fails at `the case`") && why.contains("copies of it"), "{row}");
        assert!(why.contains("judges a few transactions not judged"), "{row}");
        assert!(row.open().is_some(), "{row}");
    }

    /// `on_ping` touches no key: repeating the ping grows the keeper's
    /// state and changes nothing in `on_ping`, so the row is no effect,
    /// never a gap (C1); on both ping builds. Its id list does change
    /// `on_ping` (its count is an argument), so that row is real.
    #[test]
    fn a_receipt_the_axis_does_not_change_is_no_effect() {
        for wasm in ["keeper_ping.wasm", "keeper_ping_hashed.wasm"] {
            let h = harness(wasm, 1);
            let witness = Plan::new().op(KOp::Ping { ids: vec!["a".into()] });
            let audit = h.curves(&[witness]).run();
            eprintln!("{audit}");
            let row = audit.assert_curve(
                "keeper.near::on_ping",
                Resource::ReceiptGas,
                "repeat Ping (step 0)",
            );
            assert_eq!(row.reach, Reach::NoEffect, "{wasm}: {row}");
            assert_eq!(row.open(), None, "{wasm}: {row}");
            let ids = audit.assert_curve("keeper.near::on_ping", Resource::ReceiptGas, "/Ping/ids");
            assert_ne!(ids.reach, Reach::NoEffect, "{wasm}: {ids}");
            // The transaction's gas stays below its ceiling as far as the
            // ids moved: measured, not a gap.
            let entry = audit.assert_curve("keeper.near::ping", Resource::PrepaidGas, "/Ping/ids");
            assert_eq!(entry.open(), None, "{wasm}: {entry}");
        }
    }

    /// A direct `on_transfer` from `who` crediting `ids` to the receiver;
    /// with `notify`, telling it (the open keeper takes any caller).
    fn credit(who: &str, ids: &[String], notify: bool) -> Call {
        let msg = match notify {
            true => json!({ "receiver": RECEIVER, "notify": "hi" }),
            false => json!({ "receiver": RECEIVER }),
        };
        Call::new(who, KEEPER, "on_transfer").args_json(json!({
            "sender_id": who,
            "token_ids": ids,
            "amounts": vec!["1"; ids.len()],
            "msg": msg.to_string(),
        }))
    }

    /// `n` ids of `len` bytes, starting with `tag`.
    fn ids(tag: &str, n: usize, len: usize) -> Vec<String> {
        (0..n).map(|i| format!("{tag}{i:04}{}", "x".repeat(len.saturating_sub(5)))).collect()
    }

    /// A tail during the hold crosses from a witness of the trigger
    /// alone: a credit of 16 ids that tells the receiver. Copies
    /// of it sent while its `on_resolve` is held put entries after the
    /// credited ids, and the resolver runs out of gas at the first copy: a
    /// crossing, with a replay that shows it. All axes, none declared, no
    /// guided search. The hold makes the finding forced: a schedule the
    /// network allows, not one it was seen to take.
    #[test]
    fn a_tail_during_the_hold_crosses_from_the_trigger_alone() {
        let h = harness("keeper.wasm", 1);
        let witness = Plan::<KOp>::new().call(credit(ALICE, &ids("d", 16, 6), true));
        h.run(&witness).assert_clean();
        let audit = h.curves(&[witness]).run();
        eprintln!("{audit}");
        let tail = "keeper.near::on_transfer (step 0) while keeper.near::on_resolve is held";
        let row = audit.assert_curve(ON_RESOLVE, Resource::ReceiptGas, tail);
        assert_eq!(row.reach, Reach::Crossed { at: 1 }, "{row}");
        let path = row.replay.as_ref().unwrap_or_else(|| panic!("no replay: {row}"));
        let regressions = h.clone().store(path.parent().unwrap()).regressions();
        let mine = regressions.iter().find(|r| r.path == *path).expect("the replay");
        assert!(matches!(mine.status, Status::Shows(_)), "{row}");
        // Through the token: the tail is the keeper's `on_transfer` the
        // token called, resubmitted from a signer of its own.
        let h = harness("keeper.wasm", 16);
        let deposit = KOp::Deposit { ids: (0..16).map(|i| format!("id{i}")).collect() };
        let tail = "keeper.near::on_transfer while keeper.near::on_resolve is held";
        let audit = h.curve(&Plan::new().op(deposit), 0, tail).run();
        let row = audit.assert_curve(ON_RESOLVE, Resource::ReceiptGas, tail);
        assert_eq!(row.reach, Reach::Crossed { at: 1 }, "{row}");
    }
}

/// The custody specimen (`tests/contracts/custody`): a notified deposit
/// through the token, which the sink refunds in full, resolved in
/// `resolve_deposit`. `custody_gas_bug.wasm` gives the resolver a flat
/// 6 Tgas whatever the positions; `custody_log_bug.wasm` never checks that
/// its refund log fits the host's 16 KiB.
mod custody {
    use super::*;

    const TOKEN: &str = "token.near";
    const CUSTODY: &str = "custody.near";
    const RESOLVE: &str = "custody.near::resolve_deposit";

    fn harness(wasm: &str) -> Harness<Deposit> {
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
        Harness::new(f.build()).must_not_fail("resolve_deposit")
    }

    /// A notified deposit of `amount` over positions `ids`.
    #[derive(Clone, Debug, Serialize, Deserialize)]
    struct Deposit {
        amount: u16,
        ids: Vec<String>,
        memo: String,
    }

    impl Op for Deposit {
        fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
            let msg = json!({ "receiver": "sink.near", "ids": self.ids, "memo": self.memo, "notify": true });
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

    /// The gas bug: the resolver's own gas rises with the positions against
    /// a flat 6 Tgas, and a run crosses it; the fixed build's budget grows
    /// with them, and nothing crosses. From the plain witness, all axes.
    #[test]
    fn a_flat_budget_crosses_along_the_list() {
        let audit = harness("custody_gas_bug.wasm").curves(&[witness()]).run();
        eprintln!("{audit}");
        let row = audit.assert_curve(RESOLVE, Resource::ReceiptGas, "/ids");
        assert!(matches!(row.reach, Reach::Crossed { .. }), "{row}");
        assert!(row.replay.is_some(), "{row}");
        let fixed = harness("custody.wasm").curves(&[witness()]).run();
        let row = fixed.assert_curve(RESOLVE, Resource::ReceiptGas, "/ids");
        assert!(!matches!(row.reach, Reach::Crossed { .. }), "{row}");
    }

    /// Points from two sources combine: a curve climbed only to 2
    /// positions is open (C4) on its own; with a scan's cases of 16
    /// positions counted it moved to 16 and is closed. The scan's
    /// least-gas edge starves the resolver at 16: that crossing is the
    /// gas's, not the list's, so it is not the row's. On the gas bug the
    /// curve's own projection run crosses at 4, and the union leaves a
    /// crossing as it is. A report read back counts as its audit does.
    #[test]
    fn points_from_other_cases_count_toward_the_curve() {
        let wide = |n: usize| {
            let ids = (0..n).map(|k| format!("p{k}")).collect();
            Plan::new().op(Deposit { amount: 100, ids, memo: "m".into() })
        };
        let h = harness("custody.wasm");
        let alone = h.curve(&witness(), 0, "ids").rungs(2).run();
        let row = alone.assert_curve(RESOLVE, Resource::ReceiptGas, "/ids");
        assert_eq!((row.moved_to, row.reach.clone()), (2, Reach::Below), "{row}");
        assert!(row.open().is_some(), "{row}");
        let scan = h.scan(&[wide(16)]).run();
        let both = h.curve(&witness(), 0, "ids").rungs(2).with(&scan).run();
        let row = both.assert_curve(RESOLVE, Resource::ReceiptGas, "/ids");
        assert_eq!((row.moved_to, row.reach.clone()), (16, Reach::Below), "{row}");
        assert_eq!(row.elsewhere.last().map(|p| p.0), Some(16), "{row}");
        assert!(row.open().is_none() && row.to_string().contains("from other cases"), "{row}");
        // The same from the scan's report as saved, read back; its
        // points (storage in yoctoNEAR past u64 among them) round-trip.
        let loaded = Report::from_json(&scan.report().json()).unwrap();
        assert_eq!(loaded, scan.report());
        let again = h.curve(&witness(), 0, "ids").rungs(2).with(&loaded).run();
        let same = again.assert_curve(RESOLVE, Resource::ReceiptGas, "/ids");
        assert_eq!((same.moved_to, &same.elsewhere), (16, &row.elsewhere), "{same}");

        let h = harness("custody_gas_bug.wasm");
        let scan = h.scan(&[wide(8)]).run();
        let both = h.curve(&witness(), 0, "ids").rungs(2).with(&scan).run();
        let row = both.assert_curve(RESOLVE, Resource::ReceiptGas, "/ids");
        assert_eq!((row.moved_to, row.reach.clone()), (4, Reach::Crossed { at: 4 }), "{row}");
        assert!(row.elsewhere.is_empty(), "{row}");
        scan.assert_found("must not fail", RESOLVE);
    }

    /// The log bug: the refund log crosses the host's 16 KiB along the
    /// memo's bytes, with a replay that shows it.
    #[test]
    fn a_refund_log_crosses_the_host_limit() {
        let h = harness("custody_log_bug.wasm");
        let audit = h.curves(&[witness()]).run();
        eprintln!("{audit}");
        let row = audit.assert_curve(RESOLVE, Resource::LogBytes, "/memo");
        assert!(matches!(row.reach, Reach::Crossed { .. }), "{row}");
        assert_eq!(row.limit, 16384, "{row}");
        assert!(row.replay.is_some(), "{row}");
    }
}

/// The desk's `band` build (`desk_band.wasm`): `redeem` refuses under
/// 54.07 Tgas, then shares what is left between the issuer's `deliver`
/// and its own `release` (weight 1 each). Between the guard and the least
/// gas the redemption succeeds at, `release` runs out of gas: a window of
/// the caller's gas, found by refining where the outcomes change.
mod desk {
    use super::*;

    const DESK: &str = "desk.near";
    const ALICE: &str = "alice.near";

    #[derive(Clone, Debug, Serialize, Deserialize)]
    struct Redeem {
        tickets: Vec<String>,
        amount: String,
    }

    impl Op for Redeem {
        fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
            sim.call(ALICE, DESK, "redeem")
                .args_json(json!({ "tickets": self.tickets, "amount": self.amount }))
                .submit();
            Ok(())
        }
    }

    #[test]
    fn a_gas_window_is_found_by_refinement() {
        let f = Fixture::new()
            .account(ALICE, Balance::from_near(100))
            .stub("issuer.near", Stub::new().on("deliver", Reply::json("ok")))
            .contract(DESK, res("desk_band.wasm"))
            .init("new")
            .args_json(json!({ "issuer": "issuer.near" }))
            .call(ALICE, DESK, "deposit")
            .deposit(Balance::from_near(50));
        let h = Harness::new(f.build());
        let witness = Plan::new().op(Redeem { tickets: vec!["t1".into()], amount: "1000".into() });
        h.run(&witness).assert_clean();
        let audit = h.curve(&witness, 0, "caller gas").run();
        eprintln!("{audit}");
        let row = audit.assert_curve("desk.near::release", Resource::ReceiptGas, "caller gas");
        let Reach::Crossed { at } = row.reach else { panic!("{row}") };
        let guard = 54_070_000_000_000;
        assert!(at >= guard && at < 100_000_000_000_000, "{row}");
        assert!(row.replay.is_some(), "{row}");
        // The redemption refuses below the guard.
        let entry = audit.assert_curve("desk.near::redeem", Resource::ReceiptGas, "caller gas");
        assert!(entry.points.first().is_some_and(|p| p.0 >= guard), "{entry}");
    }
}

/// The gasmax specimen (`gasmax.wasm`): `put(id, bytes)` appends to an
/// entry; `settle(ids)` asks a ledger, then `on_settled` removes them with
/// a budget of k + m n: removing one moves the last entry's whole record
/// into its place. Repeating the put that grows the last record raises
/// `on_settled`'s gas with no crossing within 64 repeats: measured, open.
mod gasmax {
    use super::*;

    const JOURNAL: &str = "journal.near";

    #[derive(Clone, Debug, Serialize, Deserialize)]
    enum JOp {
        Put { id: String, bytes: String },
        Settle { ids: Vec<String> },
    }

    impl Op for JOp {
        fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
            let (method, args) = match self {
                JOp::Put { id, bytes } => ("put", json!({ "id": id, "bytes": bytes })),
                JOp::Settle { ids } => ("settle", json!({ "ids": ids })),
            };
            sim.call("alice.near", JOURNAL, method).args_json(args).submit();
            Ok(())
        }
    }

    /// A growing record: its gas rises with each put, measured below the
    /// limit as far as the ladder climbed, its slope positive; nothing is
    /// projected past the runs.
    #[test]
    fn a_growing_record_is_measured_not_projected() {
        let f = Fixture::new()
            .account("alice.near", Balance::from_near(1000))
            .stub("ledger.near", Stub::new().on("record", Reply::json("ok")))
            .contract(JOURNAL, res("gasmax.wasm"))
            .init("new")
            .args_json(json!({ "ledger": "ledger.near" }));
        let h = Harness::new(f.build()).must_not_fail("on_settled");
        let put = |id: &str| JOp::Put { id: id.into(), bytes: "x".repeat(64) };
        let witness =
            Plan::new().op(put("a")).op(put("b")).op(JOp::Settle { ids: vec!["a".into()] });
        let audit = h.curve(&witness, 1, "repeat Put (step 1)").run();
        eprintln!("{audit}");
        let row = audit.assert_curve(
            "journal.near::on_settled",
            Resource::ReceiptGas,
            "repeat Put (step 1)",
        );
        assert!(row.slope > 0.0, "{row}");
        assert_eq!(row.reach, Reach::Below, "{row}");
        assert!(row.to_string().contains(" a unit, at most "), "{row}");
    }
}

/// The guestbook (`guestbook.wat`): `sign` stores the caller's message
/// and asks for no deposit, so the contract stakes its bytes; `sign_paid`
/// asks for about 0.01 NEAR, which covers 900 bytes.
mod guestbook {
    use super::*;
    use common::manifest_dir;

    const BOOK: &str = "book.near";

    #[derive(Clone, Debug, Serialize, Deserialize)]
    enum BookOp {
        Sign { message: String, paid: bool },
    }

    impl Op for BookOp {
        fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
            let BookOp::Sign { message, paid } = self;
            let (method, deposit) = match paid {
                true => ("sign_paid", Balance::from_millinear(10)),
                false => ("sign", Balance::ZERO),
            };
            sim.call("alice.near", BOOK, method).args(message.clone()).deposit(deposit).submit();
            Ok(())
        }
    }

    /// The contract's stake rises with the message's bytes on `sign`; on
    /// `sign_paid` it is nothing up to 900 bytes, then rises: the deposit
    /// covers only that.
    #[test]
    fn storage_the_contract_pays_rises_with_the_message() {
        let wat = std::fs::read_to_string(manifest_dir().join("tests/res/guestbook.wat")).unwrap();
        let world = Fixture::new()
            .account("alice.near", Balance::from_near(10))
            .contract(BOOK, wat::parse_str(&wat).unwrap())
            .build();
        let h = Harness::new(world);
        let sign = |paid| Plan::new().op(BookOp::Sign { message: "hello".into(), paid });
        let audit = h.curve(&sign(false), 0, "/Sign/message").run();
        eprintln!("{audit}");
        let row = audit.assert_curve("book.near::sign", Resource::StoragePaid, "/Sign/message");
        assert!(row.slope > 0.0, "{row}");
        let audit = h.curve(&sign(true), 0, "/Sign/message").run();
        let row =
            audit.assert_curve("book.near::sign_paid", Resource::StoragePaid, "/Sign/message");
        assert!(row.points.iter().filter(|p| p.0 <= 900).all(|p| p.1 == 0), "{row}");
        assert!(row.points.iter().any(|p| p.0 > 900 && p.1 > 0), "{row}");
    }
}

/// `note` logs its input as it came: its log grows with the digits of the
/// amount it is sent as a decimal string, never past the 39 of a u128.
mod note {
    use super::*;

    const NOTE: &str = "note.near::note";
    const WAT: &str = r#"(module
      (import "env" "input" (func $input (param i64)))
      (import "env" "register_len" (func $len (param i64) (result i64)))
      (import "env" "read_register" (func $read (param i64 i64)))
      (import "env" "log_utf8" (func $log (param i64 i64)))
      (memory 1)
      (export "memory" (memory 0))
      (func (export "note")
        (call $input (i64.const 0))
        (call $read (i64.const 0) (i64.const 0))
        (call $log (call $len (i64.const 0)) (i64.const 0))))"#;

    #[derive(Clone, Debug, Serialize, Deserialize)]
    struct Note {
        amount: String,
    }

    impl Op for Note {
        fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
            sim.call("alice.near", "note.near", "note").args(self.amount.clone()).submit();
            Ok(())
        }
    }

    fn curve(amount: &str) -> near_contract_sim::explore::Audit<Note> {
        let world = Fixture::new()
            .account("alice.near", Balance::from_near(10))
            .contract("note.near", wat::parse_str(WAT).unwrap())
            .build();
        let witness = Plan::new().op(Note { amount: amount.into() });
        let audit = Harness::new(world).curve(&witness, 0, "amount").run();
        eprintln!("{audit}");
        audit
    }

    /// An amount climbed to `u128::MAX` (38 digits, then 39) runs nothing
    /// past the type: the log is below its limit there, the gas (within
    /// 2%) closed too.
    #[test]
    fn a_curve_to_the_types_max_stops_there() {
        let audit = curve(&(9 * 10u128.pow(37)).to_string());
        assert_eq!(audit.cases(), 2, "{audit}");
        let row = audit.assert_curve(NOTE, Resource::LogBytes, "/amount");
        assert_eq!((row.moved_to, &row.reach), (u128::MAX, &Reach::Below));
        let row = audit.assert_curve(NOTE, Resource::ReceiptGas, "/amount");
        assert!(row.open().is_none() && row.moved_to == u128::MAX, "{row}");
    }

    /// `note` calls its own `cb` with the amount, at a fixed 5 Tgas; `cb`
    /// loops 270,000 rounds and 6,750 more per byte of it (about 2 Tgas,
    /// and 0.05 more a digit): its gas grows with the amount's digits, so
    /// it levels off below its budget even at `u128::MAX`'s 39.
    const SPIN: &str = r#"(module
      (import "env" "input" (func $input (param i64)))
      (import "env" "register_len" (func $len (param i64) (result i64)))
      (import "env" "read_register" (func $read (param i64 i64)))
      (import "env" "current_account_id" (func $me (param i64)))
      (import "env" "promise_create"
        (func $create (param i64 i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory 1)
      (export "memory" (memory 0))
      (data (i32.const 0) "cb")
      (func (export "note") (local $n i64) (local $m i64)
        (call $input (i64.const 0))
        (local.set $n (call $len (i64.const 0)))
        (call $read (i64.const 0) (i64.const 1024))
        (call $me (i64.const 1))
        (local.set $m (call $len (i64.const 1)))
        (call $read (i64.const 1) (i64.const 512))
        (drop (call $create (local.get $m) (i64.const 512) (i64.const 2) (i64.const 0)
                            (local.get $n) (i64.const 1024) (i64.const 64)
                            (i64.const 5000000000000))))
      (func (export "cb") (local $n i64)
        (call $input (i64.const 0))
        (local.set $n
          (i64.add (i64.const 270000) (i64.mul (call $len (i64.const 0)) (i64.const 6750))))
        (block (loop
          (br_if 1 (i64.eqz (local.get $n)))
          (local.set $n (i64.sub (local.get $n) (i64.const 1)))
          (br 0)))))"#;

    /// A gas that grows with a number's digits levels off along it: below
    /// its limit as far as the amount climbed, closed: measured, nothing
    /// projected.
    #[test]
    fn a_plateau_along_a_number_is_closed() {
        let world = Fixture::new()
            .account("alice.near", Balance::from_near(10))
            .contract("note.near", wat::parse_str(SPIN).unwrap())
            .build();
        let witness = Plan::new().op(Note { amount: "500".into() });
        let audit = Harness::new(world).curve(&witness, 0, "amount").run();
        let row = audit.assert_curve("note.near::cb", Resource::ReceiptGas, "/amount");
        assert!(row.reach == Reach::Below && row.open().is_none(), "{row}");
    }

    /// The log grows with the amount's digits, far below the host's limit
    /// as far as the amount climbs: closed, nothing to replay.
    #[test]
    fn log_bytes_along_an_amount_close_below_the_limit() {
        let audit = curve("500");
        let row = audit.assert_curve(NOTE, Resource::LogBytes, "/amount");
        assert!(row.reach == Reach::Below && row.open().is_none(), "{row}");
    }
}

/// `open_registry.wat`: any caller's `register` stores a 1000-byte entry.
/// Curves copy the entry call from fresh signers ("fresh callers"): the
/// row shows the bytes each new caller costs the registry and the ratio
/// to what the caller paid; above 10x it is a finding. The paying build
/// takes the stake as a deposit: no finding, a ratio under 1.
mod fresh_callers {
    use super::*;

    const REGISTRY: &str = "registry.near";
    const AXIS: &str = "repeat fresh callers of registry.near::register";

    fn harness(charge: bool) -> Harness<Call> {
        let text =
            std::fs::read_to_string(common::manifest_dir().join("tests/res/open_registry.wat"));
        let mut text = text.unwrap();
        if charge {
            let panic = "(if (i64.lt_u (i64.load (i32.const 72)) (i64.const 600)) \
                         (then (call $panic_utf8 (i64.const 17) (i64.const 0))))";
            text = text.replace(";; {charge}", panic);
        }
        let f = Fixture::new()
            .account("alice.near", Balance::from_near(100))
            .contract(REGISTRY, ::wat::parse_str(&text).unwrap());
        Harness::new(f.build())
    }

    #[test]
    fn a_registry_staking_for_any_caller_is_found() {
        const FOUND: &str = "storage paid by the contract";
        let register = |near: u128| {
            Call::new("alice.near", REGISTRY, "register").deposit(Balance::from_millinear(near))
        };
        let witness = Plan::new().op(register(0));
        let audit = harness(false).curve(&witness, 0, AXIS).run();
        let site = "registry.near::register (fresh callers)";
        let row = audit.assert_curve(site, Resource::StoragePaid, AXIS);
        assert!(matches!(row.reach, Reach::Crossed { at: 1 }), "{row}");
        assert!(row.to_string().contains("1053 bytes a new caller"), "{row}");
        let f = audit.found().iter().find(|f| f.finding.check == FOUND);
        let f = f.unwrap_or_else(|| panic!("{audit}"));
        assert!(f.finding.message.contains("bytes each"), "{}", f.finding.message);
        assert!(row.replay.is_some(), "{row}");
        // Its own kind, in the report and its summary: not a failed receipt.
        let report = audit.report();
        let kinds = report.findings.iter().filter(|r| r.check == FOUND).map(|r| r.kind.as_str());
        assert_eq!(kinds.collect::<Vec<_>>(), ["storage paid"], "{audit}");
        assert!(
            audit.to_string().contains(&format!("[storage paid] {FOUND}")),
            "{}",
            audit.to_string()
        );
        let paying = harness(true).curve(&Plan::new().op(register(12)), 0, AXIS).run();
        assert!(paying.found().iter().all(|f| f.finding.check != FOUND), "{paying}");
        let row = paying.assert_curve(site, Resource::StoragePaid, AXIS);
        assert!(!matches!(row.reach, Reach::Crossed { .. }), "{row}");
    }
}

/// A members-only club: `join` records its caller, `act` admits one
/// guest and refuses any other caller that hasn't joined. The op picks its signer by index, naming no
/// account, so a fresh callers copy is the entry call from a made-up
/// account no op can make join: the row closes at that
/// bound, saying the op's signer can't be varied, and asks nothing of the
/// signer the op can't express.
mod fresh_signer_by_index {
    use super::*;

    const CLUB: &str = "club.near";
    const AXIS: &str = "repeat fresh callers of club.near::act";
    const MEMBERS: [&str; 2] = ["alice.near", "bob.near"];
    const WAT: &str = r#"(module
      (import "env" "predecessor_account_id" (func $pred (param i64)))
      (import "env" "register_len" (func $len (param i64) (result i64)))
      (import "env" "read_register" (func $read (param i64 i64)))
      (import "env" "storage_write" (func $write (param i64 i64 i64 i64 i64) (result i64)))
      (import "env" "storage_has_key" (func $has (param i64 i64) (result i64)))
      (import "env" "panic_utf8" (func $panic (param i64 i64)))
      (memory 1)
      (export "memory" (memory 0))
      (data (i32.const 0) "not a member")
      (data (i32.const 16) "guest")
      (func $caller (result i64)
        (call $pred (i64.const 0))
        (call $read (i64.const 0) (i64.const 64))
        (call $len (i64.const 0)))
      (func (export "join")
        (drop (call $write (call $caller) (i64.const 64) (i64.const 1) (i64.const 0)
                           (i64.const 1))))
      (func (export "act")
        (if (i64.eqz (call $has (call $caller) (i64.const 64)))
          (then
            (if (i64.eqz (call $has (i64.const 5) (i64.const 16)))
              (then (drop (call $write (i64.const 5) (i64.const 16) (i64.const 1) (i64.const 0)
                                       (i64.const 1))))
              (else (call $panic (i64.const 12) (i64.const 0))))))))"#;

    /// `act`, signed by member `who`.
    #[derive(Clone, Debug, Serialize, Deserialize)]
    struct Act {
        who: u8,
    }

    impl Op for Act {
        fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
            sim.call(MEMBERS[usize::from(self.who) % 2], CLUB, "act").submit();
            Ok(())
        }
    }

    /// The club, admitting one guest, or (`guest` false) none.
    fn club<O: Op>(guest: bool) -> Harness<O> {
        let wat = match guest {
            true => WAT.to_owned(),
            false => {
                WAT.replace("(i64.eqz (call $has (i64.const 5) (i64.const 16)))", "(i32.const 0)")
            }
        };
        let mut f = Fixture::new().contract(CLUB, ::wat::parse_str(wat).unwrap());
        for m in MEMBERS {
            f = f.account(m, Balance::from_near(10)).call(m, CLUB, "join").args_json(json!({}));
        }
        Harness::new(f.build())
    }

    #[test]
    fn a_signer_picked_by_index_closes_the_row() {
        let audit = club(true).curve(&Plan::new().op(Act { who: 0 }), 0, AXIS).run();
        let rows: Vec<_> = audit.curves().iter().filter(|r| r.axis.name == AXIS).collect();
        assert!(rows.iter().all(|r| !matches!(r.reach, Reach::Stopped(_))), "{audit}");
        let closed = rows.iter().filter(
            |r| matches!(&r.reach, Reach::AtBound(why) if why.contains("names no account")),
        );
        assert!(closed.count() > 0, "{audit}");
        assert!(rows.iter().all(|r| r.open().is_none()), "{audit}");
        let asks = |s: &str| s.contains("give its signer");
        assert!(audit.todo().iter().all(|t| !asks(&t.why)), "{audit}");
    }

    /// Refused at its first copy, the axis is never silent: with the
    /// op naming no account it closes at that bound; with an op naming its
    /// signer (`alice.near`, made fresh and funded, never a member) it is
    /// an open row saying the first copy was refused, and why.
    #[test]
    fn an_axis_refused_at_its_first_copy_has_a_row() {
        let audit = club(false).curve(&Plan::new().op(Act { who: 0 }), 0, AXIS).run();
        let rows: Vec<_> = audit.curves().iter().filter(|r| r.axis.name == AXIS).collect();
        let bound = |r: &Reach| matches!(r, Reach::AtBound(w) if w.contains("names no account"));
        assert!(rows.iter().any(|r| bound(&r.reach)), "{audit}");
        assert!(rows.iter().all(|r| r.open().is_none()), "{audit}");
        let named = Plan::new().op(Call::new("alice.near", CLUB, "act"));
        let audit = club::<Call>(false).curve(&named, 0, AXIS).run();
        let rows: Vec<_> = audit.curves().iter().filter(|r| r.axis.name == AXIS).collect();
        let open = |r: &Reach| matches!(r, Reach::Stopped(w) if w.contains("copy 1 failed") && w.contains("not a member"));
        assert!(rows.iter().any(|r| open(&r.reach)), "{audit}");
        assert!(audit.todo().iter().any(|t| t.id.ends_with(AXIS)), "{audit}");
    }

    /// `act` by an index, or by a signer it names.
    #[derive(Clone, Debug, Serialize, Deserialize)]
    enum Mixed {
        ByIndex { who: u8 },
        Named { signer: String },
    }

    impl Op for Mixed {
        fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
            let signer = match self {
                Mixed::ByIndex { who } => MEMBERS[usize::from(*who) % 2].to_owned(),
                Mixed::Named { signer } => signer.clone(),
            };
            sim.call(signer, CLUB, "act").submit();
            Ok(())
        }
    }

    /// The closing reads the climbed step's copies:
    /// a witness that already holds a fresh caller's copy of an op naming
    /// its signer (a replayed curve point) doesn't make the climbed op,
    /// which names none, look as if it named one.
    #[test]
    fn the_closing_reads_the_climbed_steps_copies() {
        let mut witness = serde_json::to_value(
            Plan::new().call(Call::new("guest-0.near", CLUB, "act")).op(Mixed::ByIndex { who: 0 }),
        )
        .unwrap();
        witness["steps"][0]["call"]["copies"] =
            json!({ "source": 0, "k": 1, "fresh": true, "signer": "alice.near" });
        witness["sources"] = json!([{ "Named": { "signer": "alice.near" } }]);
        let witness = Plan::<Mixed>::from_json(&witness.to_string()).unwrap();
        let audit = club::<Mixed>(true).curve(&witness, 1, AXIS).run();
        let rows: Vec<_> = audit.curves().iter().filter(|r| r.axis.name == AXIS).collect();
        let bound = |r: &Reach| matches!(r, Reach::AtBound(w) if w.contains("names no account"));
        let measured = rows.iter().filter(|r| r.reach != Reach::NoEffect);
        let reaches: Vec<_> = measured.map(|r| r.reach.clone()).collect();
        assert!(!reaches.is_empty() && reaches.iter().all(bound), "{reaches:?}");
    }
}

/// Copies the contract refuses at its guards. `vault.wat`
/// is laid out as a Rust contract built with panic = "abort": every
/// refusal traps with the same `unreachable`. `credit(n)`, n one byte,
/// refuses any caller but `alice.near` and `router.near`
/// (`vault::assert_caller`), n = 11 (`vault::assert_unlocked`) and n of
/// 12 or more (`vault::assert_owner`). `router.near::go` credits n = 0
/// and notes it to `stub.near`.
mod guards {
    use super::*;
    use near_contract_sim::explore::Axes;

    const VAULT: &str = "vault.near";
    const CREDIT: &str = "vault.near::credit";
    const WAT: &str = r#"(module
      (import "env" "input" (func $input (param i64)))
      (import "env" "read_register" (func $read_register (param i64 i64)))
      (import "env" "register_len" (func $register_len (param i64) (result i64)))
      (import "env" "predecessor_account_id" (func $predecessor (param i64)))
      (import "env" "log_utf8" (func $log_utf8 (param i64 i64)))
      (memory 1)
      (export "memory" (memory 0))
      (func $std::process::abort unreachable)
      (func $core::panicking::panic (call $std::process::abort))
      (func $vault::assert_caller (local $n i64)
        (call $predecessor (i64.const 1))
        (local.set $n (call $register_len (i64.const 1)))
        (if (i32.and (i64.ne (local.get $n) (i64.const 10)) (i64.ne (local.get $n) (i64.const 11)))
          (then (call $core::panicking::panic))))
      (func $vault::assert_unlocked (param $n i32)
        (if (i32.eq (local.get $n) (i32.const 11)) (then (call $core::panicking::panic))))
      (func $vault::assert_owner (param $n i32)
        (if (i32.ge_u (local.get $n) (i32.const 12)) (then (call $core::panicking::panic))))
      (func $credit (export "credit") (local $n i32)
        (call $vault::assert_caller)
        (call $input (i64.const 0))
        (call $read_register (i64.const 0) (i64.const 256))
        (local.set $n (i32.load8_u (i32.const 256)))
        (call $vault::assert_unlocked (local.get $n))
        (call $vault::assert_owner (local.get $n))))"#;
    const ROUTER: &str = r#"(module
      (import "env" "promise_create"
        (func $create (param i64 i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory 1)
      (export "memory" (memory 0))
      (data (i32.const 0) "vault.near")
      (data (i32.const 16) "credit")
      (data (i32.const 32) "stub.near")
      (data (i32.const 48) "note")
      (data (i32.const 100) "{}")
      (func (export "go")
        (drop (call $create (i64.const 10) (i64.const 0) (i64.const 6) (i64.const 16)
                            (i64.const 1) (i64.const 96) (i64.const 64) (i64.const 20000000000000)))
        (drop (call $create (i64.const 9) (i64.const 32) (i64.const 4) (i64.const 48)
                            (i64.const 2) (i64.const 100) (i64.const 64) (i64.const 10000000000000)))))"#;

    fn world() -> Fixture {
        Fixture::new()
            .account("alice.near", Balance::from_near(100))
            .contract(VAULT, wat::parse_str(WAT).unwrap())
            .contract("router.near", wat::parse_str(ROUTER).unwrap())
            .stub("stub.near", Stub::new().on("note", Reply::json("ok")))
    }

    /// Alice credits `nonce`. Copy k of it credits 10 + k, or 12 + k if
    /// `far`: copy 1 passes and copy 2 is locked, or copy 1 isn't the
    /// owner's. Only its repeats are axes.
    #[derive(Clone, Debug, Serialize, Deserialize)]
    struct Credit {
        nonce: u8,
        far: bool,
    }

    impl Op for Credit {
        fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
            sim.call("alice.near", VAULT, "credit").args(vec![self.nonce]).submit();
            Ok(())
        }

        fn axes(&self) -> Axes {
            Axes::only(["repeat Credit"])
        }

        fn copy(&self, k: usize) -> Self {
            let base = if self.far { 12 } else { 10 };
            Credit { nonce: base + k as u8, far: self.far }
        }
    }

    fn credit(far: bool) -> Credit {
        Credit { nonce: 0, far }
    }

    /// A raw inner receipt is re-sent only into an audited contract: with
    /// `audits`, the router's note to the stub is no repeat axis; without,
    /// it is. The credit into the audited vault stays one,
    /// and so does its direct witness (its refusal is the `list:` item).
    ///
    /// Verification boundary: which curve axes a witness has, over an
    /// actual run.
    #[test]
    fn raw_copies_go_only_into_audited_contracts() {
        let witness = Plan::new().op(Call::new("alice.near", "router.near", "go"));
        let names = |h: &Harness<Call>| -> Vec<String> {
            h.axes(&witness).into_iter().map(|(_, a)| a.name).collect()
        };
        let all = Harness::<Call>::new(world().build());
        let audited = all.clone().audits(["router.near", VAULT]);
        let (every, mine) = (names(&all), names(&audited));
        assert!(every.iter().any(|a| a == "repeat stub.near::note"), "{every:?}");
        assert!(!mine.iter().any(|a| a.contains("stub.near")), "{mine:?}");
        assert!(mine.iter().any(|a| a == "repeat vault.near::credit"), "{mine:?}");
        assert!(mine.iter().any(|a| a == "repeat fresh callers of router.near::go"), "{mine:?}");
        let audit = audited.curves(std::slice::from_ref(&witness)).rungs(2).run();
        let todo = audit.todo();
        assert!(todo.iter().any(|t| t.id == format!("list:{CREDIT}")), "{todo:#?}");
    }

    /// A stop names the function its trap is in and both callers: the
    /// router's credit re-sent from `axis.near` fails
    /// `vault::assert_caller` at copy 1. On a panic = "abort" build every
    /// refusal reads `unreachable` unnamed, a guard and a missing
    /// precondition alike.
    ///
    /// Verification boundary: a curve's stop over actual runs, its trap
    /// named from a rerun on an instrumented copy.
    #[test]
    fn a_stop_names_its_trap_and_both_callers() {
        let h = Harness::<Call>::new(world().build());
        let witness = Plan::new().op(Call::new("alice.near", "router.near", "go"));
        let axis = "repeat vault.near::credit";
        let audit = h.curve(&witness, 0, axis).run();
        let row = audit.assert_curve(CREDIT, Resource::ReceiptGas, axis);
        let Reach::Stopped(why) = &row.reach else { panic!("{row}") };
        assert!(why.contains("copy 1 failed at `vault.near::credit`"), "{why}");
        assert!(why.contains("executed in `vault::assert_caller`, called from `credit`"), "{why}");
        assert!(why.contains("axis.near") && why.contains("router.near"), "{why}");
        let id = format!("axis:step 0:{axis}");
        let todo = audit.todo();
        let item = todo.iter().find(|t| t.id == id).unwrap_or_else(|| panic!("{todo:#?}"));
        assert!(item.why.contains("`vault::assert_caller`"), "{item:?}");
        // The hint's excuse is the item's own id: applied, it closes it.
        let hint = item.why.split(".excuse(\"").nth(1).and_then(|s| s.split('"').next());
        let hint = hint.unwrap_or_else(|| panic!("{item:?}"));
        assert_eq!(hint, id, "{item:?}");
        let excused = h.clone().excuse(hint, "a caller check").union(&[audit.report()]);
        assert!(!excused.todo.iter().any(|t| t.id == id), "{excused}");
        assert_eq!(excused.excused[0].2, std::slice::from_ref(&id), "{excused:#?}");
    }

    /// The stop is the first copy that failed, not the last:
    /// at 4 copies, copy 2 is locked and copies 3 and 4 aren't the
    /// owner's; the row says copy 2 and `vault::assert_unlocked`.
    ///
    /// Verification boundary: a curve's stop over actual runs.
    #[test]
    fn a_stop_is_the_first_copy_that_failed() {
        let h = Harness::<Credit>::new(world().build());
        let axis = "repeat Credit (step 0)";
        let audit = h.curve(&Plan::new().op(credit(false)), 0, axis).run();
        let row = audit.assert_curve(CREDIT, Resource::ReceiptGas, axis);
        let Reach::Stopped(why) = &row.reach else { panic!("{row}") };
        assert!(why.starts_with("at 4 copies, copy 2 failed at `vault.near::credit`"), "{why}");
        assert!(why.contains("`vault::assert_unlocked`"), "{why}");
        assert!(!why.contains("assert_owner"), "{why}");
        assert!(why.contains("alice.near"), "{why}");
    }

    /// The summary groups stopped axes by the site and the function that
    /// refused them: two axes stopped by the lock are one line,
    /// one excuse closes both, and both rows stay stopped in the report;
    /// a different guard at the same site is a line of its own. The
    /// to-do ids stay one an axis. A group no glob closes exactly asks
    /// for an excuse for each id.
    ///
    /// Verification boundary: the report's summary and to-do list over
    /// actual runs, read back from its JSON.
    #[test]
    fn stops_group_by_site_and_guard() {
        let h = Harness::<Credit>::new(world().build());
        let twice = Plan::new().op(credit(false)).op(credit(false));
        let report = Report::from_json(&h.curves(&[twice]).run().report().json()).unwrap();
        let ids = ["axis:step 0:repeat Credit (step 0)", "axis:step 1:repeat Credit (step 1)"];
        for id in ids {
            assert!(report.todo.iter().any(|t| t.id == id), "{:#?}", report.todo);
        }
        let stopped = report.curves.iter().filter(|c| matches!(c.reach, Reach::Stopped(_)));
        let axes: std::collections::BTreeSet<_> = stopped.map(|c| (c.step, &c.axis)).collect();
        assert_eq!(axes.len(), 2, "{report:#?}");
        let summary = report.to_string();
        let lines: Vec<&str> = summary.lines().filter(|l| l.contains("assert_unlocked")).collect();
        assert_eq!(lines.len(), 1, "{summary}");
        assert!(lines[0].contains("2 axes") && lines[0].contains(CREDIT), "{summary}");
        assert!(!summary.contains("TODO axis:"), "{summary}");
        let glob = lines[0].split(".excuse(\"").nth(1).and_then(|s| s.split('"').next());
        let glob = glob.unwrap_or_else(|| panic!("{summary}"));
        // Each place the ids differ, starred: no wider than the group.
        assert_eq!(glob, "axis:step *:repeat Credit (step *)", "{summary}");
        let excused = h.clone().excuse(glob, "a lock while held").union(&[report]);
        assert!(!excused.todo.iter().any(|t| t.id.starts_with("axis:")), "{excused}");
        assert_eq!(excused.excused[0].2.len(), 2, "{excused:#?}");

        let two = Plan::new().op(credit(false)).op(credit(true));
        let summary = h.curves(&[two]).run().report().to_string();
        let lines = |f: &str| summary.lines().filter(|l| l.contains(f)).count();
        assert_eq!(lines("`vault::assert_unlocked`"), 1, "{summary}");
        assert_eq!(lines("`vault::assert_owner`"), 1, "{summary}");

        // A glob that would also close another open item is never offered:
        // steps 0 and 2's lock group would star over step 1's owner stop.
        let around = Plan::new().op(credit(false)).op(credit(true)).op(credit(false));
        let summary = h.curves(&[around]).run().report().to_string();
        let line = summary.lines().find(|l| l.contains("`vault::assert_unlocked`"));
        let line = line.unwrap_or_else(|| panic!("{summary}"));
        assert!(line.starts_with("stopped: 2 axes"), "{summary}");
        assert!(line.contains("a guard? an excuse for each id;"), "{summary}");
        assert!(!line.contains(".excuse("), "{summary}");
    }

    /// Alice lends, loans or borrows `nonce`, a credit each, copied as
    /// [`Credit`] is: copy 2 is locked.
    #[derive(Clone, Debug, Serialize, Deserialize)]
    enum Lent {
        Lend { nonce: u8 },
        Loan { nonce: u8 },
        Borrow { nonce: u8 },
    }

    impl Op for Lent {
        fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
            let (Lent::Lend { nonce } | Lent::Loan { nonce } | Lent::Borrow { nonce }) = self;
            sim.call("alice.near", VAULT, "credit").args(vec![*nonce]).submit();
            Ok(())
        }

        fn axes(&self) -> Axes {
            Axes::only(["repeat Lend", "repeat Loan", "repeat Borrow"])
        }

        fn copy(&self, k: usize) -> Self {
            let nonce = 10 + k as u8;
            match self {
                Lent::Lend { .. } => Lent::Lend { nonce },
                Lent::Loan { .. } => Lent::Loan { nonce },
                Lent::Borrow { .. } => Lent::Borrow { nonce },
            }
        }
    }

    /// Two ops the lock stops are one group, and its glob stars each run
    /// of places their ids differ once: `L*` for `Lend` and `Loan`. Ids
    /// of different lengths keep their common head and tail around one
    /// star, checked to close no other open item.
    ///
    /// Verification boundary: the summary over actual runs.
    #[test]
    fn a_group_s_glob_stars_each_difference_once() {
        let h = Harness::<Lent>::new(world().build());
        let glob = |second: Lent| {
            let plan = Plan::new().op(Lent::Lend { nonce: 0 }).op(second);
            let summary = h.curves(&[plan]).run().to_string();
            let line = summary.lines().find(|l| l.starts_with("stopped: 2 axes"));
            let line = line.unwrap_or_else(|| panic!("{summary}"));
            let glob = line.split(".excuse(\"").nth(1).and_then(|s| s.split('"').next());
            glob.unwrap_or_else(|| panic!("{summary}")).to_owned()
        };
        assert_eq!(glob(Lent::Loan { nonce: 0 }), "axis:step *:repeat L* (step *)");
        assert_eq!(glob(Lent::Borrow { nonce: 0 }), "axis:step *)");
    }

    /// Alice credits `n`, then 0: the step's second call always passes.
    #[derive(Clone, Debug, Serialize, Deserialize)]
    struct Then {
        n: u8,
    }

    impl Op for Then {
        fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
            sim.call("alice.near", VAULT, "credit").args(vec![self.n]).submit();
            sim.call("alice.near", VAULT, "credit").args(vec![0]).submit();
            Ok(())
        }

        fn axes(&self) -> Axes {
            Axes::only(["/n"])
        }
    }

    /// Only copy axes group: an argument axis stopped at a
    /// guard is a to-do line of its own, never a `stopped:` group, though
    /// its refusal has a site and a named trap. Here `n` climbs into the
    /// vault's guards while the step's other credit passes, so the step
    /// isn't refused whole and the row stays open.
    ///
    /// Verification boundary: a curve's stop and the summary, over
    /// actual runs.
    #[test]
    fn an_argument_axis_keeps_its_own_line() {
        let h = Harness::<Then>::new(world().build());
        let audit = h.curve(&Plan::new().op(Then { n: 1 }), 0, "n").run();
        let summary = audit.to_string();
        let id = "axis:step 0:/n";
        let item = audit.todo().into_iter().find(|t| t.id == id);
        let item = item.unwrap_or_else(|| panic!("{summary}"));
        assert!(item.why.contains("fails at `vault.near::credit`"), "{item:?}");
        assert!(item.why.contains("executed in `vault::assert_"), "{item:?}");
        assert!(summary.lines().any(|l| l.starts_with(&format!("TODO {id}: "))), "{summary}");
        assert!(!summary.contains("stopped: "), "{summary}");
    }

    /// `Axes::only` says which repeats it left out: a narrowed
    /// op's dropped copy axes are listed, as strings that aren't axes are.
    ///
    /// Verification boundary: which curve axes a witness has, and the
    /// summary, over actual runs.
    #[test]
    fn axes_only_lists_the_repeats_it_left_out() {
        let h = Harness::<Credit>::new(world().build());
        let audit = h.curves(&[Plan::new().op(credit(false))]).rungs(2).run();
        let summary = audit.to_string();
        let line = summary.lines().find(|l| l.starts_with("left out by Axes::only"));
        let line = line.unwrap_or_else(|| panic!("{summary}"));
        assert!(line.contains("repeat fresh callers of vault.near::credit"), "{line}");
        assert!(!line.contains("repeat Credit"), "{line}");
        let back = Report::from_json(&audit.report().json()).unwrap();
        assert!(back.to_string().contains(line), "{back}");
    }
}

/// `spin(n)`, n a little-endian u32, hashes 10 KiB `n` times: about 0.3
/// Tgas each, so past some 3,300 rounds no transaction carries it (at
/// the 1,000 Tgas ceiling).
mod spin {
    use super::*;
    use near_contract_sim::Gas;
    use near_contract_sim::explore::{Axes, StepGas};

    const SPIN: &str = "spin.near";
    const WAT: &str = r#"(module
      (import "env" "input" (func $input (param i64)))
      (import "env" "read_register" (func $read (param i64 i64)))
      (import "env" "sha256" (func $sha256 (param i64 i64 i64)))
      (memory 1)
      (export "memory" (memory 0))
      (func (export "spin") (local $n i32)
        (call $input (i64.const 0))
        (call $read (i64.const 0) (i64.const 0))
        (local.set $n (i32.load (i32.const 0)))
        (block $done
          (loop $round
            (br_if $done (i32.eqz (local.get $n)))
            (call $sha256 (i64.const 10240) (i64.const 0) (i64.const 1))
            (local.set $n (i32.sub (local.get $n) (i32.const 1)))
            (br $round)))))"#;

    /// Spins `n` rounds; with `PIN`, at the 100 Tgas the op attaches.
    #[derive(Clone, Debug, Serialize, Deserialize)]
    struct Spin<const PIN: bool> {
        n: u32,
    }

    impl<const PIN: bool> Op for Spin<PIN> {
        fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
            let call = sim.call("alice.near", SPIN, "spin").args(self.n.to_le_bytes());
            match PIN {
                true => call.gas(Gas::from_teragas(100)).submit(),
                false => call.submit(),
            };
            Ok(())
        }

        fn axes(&self) -> Axes {
            Axes::only(["/n"])
        }
    }

    fn world() -> Sim {
        let f = Fixture::new()
            .account("alice.near", Balance::from_near(100))
            .contract(SPIN, wat::parse_str(WAT).unwrap());
        f.build()
    }

    /// A call whose own gas grows along its argument runs, unpinned, at
    /// the ceiling's gas, and its entry running out of it is a bound of
    /// the transaction, closed: no cap or guard to look for. The row's
    /// plan, to replay, carries that gas. A call pinning its own gas
    /// keeps it: its row's plan sets none.
    ///
    /// Verification boundary: a curve's ladder and stop over actual runs.
    #[test]
    fn an_entry_out_of_gas_is_the_transaction_s_bound() {
        let witness = Plan::new().op(Spin::<false> { n: 8 });
        let audit = Harness::new(world()).curve(&witness, 0, "/n").run();
        let row = audit.assert_curve("spin.near::spin", Resource::ReceiptGas, "/n");
        let Reach::AtBound(why) = &row.reach else { panic!("{row}") };
        assert!(why.contains("out of gas"), "{row}");
        assert!(why.contains("past what one transaction carries"), "{row}");
        assert_eq!(row.open(), None, "{row}");
        let ceiling = Some(StepGas::Exactly(world().gas_ceiling()));
        assert_eq!(row.plan.steps[0].gas, ceiling, "{row}");

        let pinned = Plan::new().op(Spin::<true> { n: 8 });
        let audit = Harness::new(world()).curve(&pinned, 0, "/n").run();
        let row = audit.assert_curve("spin.near::spin", Resource::ReceiptGas, "/n");
        assert!(row.moved_to > 8, "{row}");
        assert_eq!(row.plan.steps[0].gas, None, "{row}");
    }
}

/// `go` writes `bad`.
const GOES: &str = r#"(module
  (import "env" "storage_write" (func $sw (param i64 i64 i64 i64 i64) (result i64)))
  (memory 1)
  (data (i32.const 0) "bad")
  (func (export "go")
    (drop (call $sw (i64.const 3) (i64.const 0) (i64.const 3) (i64.const 0) (i64.const 1)))))"#;

/// Calls `go` once per `times`, at most 3.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Go {
    times: u64,
}

impl Op for Go {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        for _ in 0..self.times.clamp(1, 3) {
            sim.call("alice.near", "c.near", "go").submit();
        }
        Ok(())
    }
}

/// Curves save their report at the end, then their replays, as a scan
/// does: the saves file's rows are the audit's, a fold
/// once, not a folded audit folded again.
///
/// Verification boundary: curves' saves file over actual runs.
#[test]
fn curves_save_the_rows_they_report() {
    let sim = Fixture::new()
        .account("alice.near", Balance::from_near(10))
        .contract("c.near", wat::parse_str(GOES).unwrap())
        .build();
    let bad = |sim: &Sim| match sim.state("c.near", b"bad") {
        Some(_) => Err("bad was written".to_owned()),
        None => Ok(()),
    };
    let dir = std::env::temp_dir().join(format!("curves-rows-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let h = Harness::<Go>::new(sim)
        .store(dir.join("found"))
        .at_rest("nothing bad", bad)
        .at_rest("nothing written", bad);
    let path = dir.join("saves.json");
    let audit = h.curves(&[Plan::new().op(Go { times: 1 })]).saves(&path, 1).run();
    // Each row with the other check folded in, once.
    assert!(!audit.found().is_empty(), "{audit}");
    assert!(audit.found().iter().all(|x| x.also.len() == 1), "{audit}");
    // Rows as JSON, the replays a saves file carries aside.
    let rows = |r: &Report| {
        let mut v = serde_json::to_value(&r.findings).unwrap();
        v.as_array_mut().unwrap().iter_mut().for_each(|x| {
            x.as_object_mut().unwrap().remove("saved");
        });
        v
    };
    let saved = Report::from_json(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(rows(&saved), rows(&audit.report()));
    let replays = audit.found().iter().map(|x| x.replay.as_ref().expect("saved"));
    assert!(replays.into_iter().all(|p| p.exists()), "{audit}");
    let _ = std::fs::remove_dir_all(&dir);
}
