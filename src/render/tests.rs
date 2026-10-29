use super::*;
use crate::outcome::plain_ids;
use near_primitives::errors::{
    ActionError, ActionErrorKind, FunctionCallError, InvalidTxError, TxExecutionError,
};
use near_primitives::receipt::DataReceiver;
use near_primitives::transaction::ExecutionStatus;
use near_primitives::types::Balance;

fn id(n: u8) -> CryptoHash {
    CryptoHash([n; 32])
}

fn ro(n: u8, executor: &str) -> ReceiptOutcome {
    ReceiptOutcome {
        receipt_id: id(n),
        tx: id(0),
        receiver_id: executor.parse().unwrap(),
        predecessor_id: "alice.near".parse().unwrap(),
        method_names: vec![],
        args: vec![],
        actions: vec![],
        refund: None,
        block_height: 1,
        block_timestamp_ns: 0,
        gas_burnt: Gas::ZERO,
        tokens_burnt: Balance::ZERO,
        logs: vec![],
        status: ExecutionStatus::SuccessValue(vec![]),
        receipt_ids: vec![],
        gas_profile: Default::default(),
        output_data_receivers: vec![],
        input_data_ids: vec![],
        attached_gas: Gas::ZERO,
        deposit: Balance::ZERO,
        prepaid_exec_fees: Gas::ZERO,
        unhandled: false,
        delivered: false,
        fault: None,
        held: None,
        result_held: None,
        token_moves: vec![],
        storage: vec![],
        forwarded: Gas::ZERO,
        storage_ops: Default::default(),
        exports: None,
    }
}

fn missing(a: &str) -> ExecutionStatus {
    let kind = ActionErrorKind::AccountDoesNotExist { account_id: a.parse().unwrap() };
    ExecutionStatus::Failure(TxExecutionError::ActionError(ActionError { index: Some(0), kind }))
}

/// A tx whose call returns a promise: a handled failed transfer (its
/// result feeds the callback), the callback on the result path, and an
/// unhandled failed transfer the callback started. Plus a refund.
fn sample(with_pending: bool) -> TxOutcome {
    let mut tx = ro(0, "alice.near");
    tx.gas_burnt = Gas::from_gas(100_000_000_000);
    tx.status = ExecutionStatus::SuccessReceiptId(id(1));
    tx.receipt_ids = vec![id(1)];
    let mut call = ro(1, "app.near");
    call.method_names = vec!["run".into()];
    call.actions = vec!["FunctionCall".into()];
    call.attached_gas = Gas::from_teragas(10);
    call.gas_burnt = Gas::from_teragas(2);
    call.prepaid_exec_fees = Gas::from_gas(500_000_000_000);
    call.block_height = 2;
    call.status = ExecutionStatus::SuccessReceiptId(id(3));
    call.receipt_ids = vec![id(2), id(3), id(9)];
    let mut bob = ro(2, "bob.near");
    bob.actions = vec!["Transfer".into()];
    bob.deposit = Balance::from_near(1);
    bob.gas_burnt = Gas::from_gas(223_182_562_500);
    bob.status = missing("bob.near");
    bob.block_height = 3;
    bob.output_data_receivers =
        vec![DataReceiver { data_id: id(20), receiver_id: "app.near".parse().unwrap() }];
    let mut cb = ro(3, "app.near");
    cb.method_names = vec!["cb".into()];
    cb.actions = vec!["FunctionCall".into()];
    cb.status = ExecutionStatus::SuccessValue(b"true".to_vec());
    cb.input_data_ids = vec![id(20), id(21)];
    cb.attached_gas = Gas::from_teragas(5);
    cb.gas_burnt = Gas::from_teragas(1);
    cb.block_height = 4;
    cb.receipt_ids = if with_pending { vec![id(4), id(5)] } else { vec![id(4)] };
    let mut carol = ro(4, "carol.near");
    carol.status = missing("carol.near");
    carol.block_height = 5;
    carol.unhandled = true;
    let mut refund = ro(9, "alice.near");
    refund.refund = Some(crate::Refund::Gas);
    refund.predecessor_id = "system".parse().unwrap();
    refund.status = missing("alice.near"); // never reported
    TxOutcome {
        hash: id(0),
        transaction: Some(tx),
        receipts: vec![call, bob, cb, carol, refund],
        status: FinalExecutionStatus::SuccessValue(vec![]),
        pending: vec![],
        effects: Default::default(),
    }
}

#[test]
fn formatting_helpers() {
    assert_eq!(short(&CryptoHash::default()), "111111…");
    assert_eq!(tgas(Gas::from_gas(1_234_567_890_123)), "1.23 Tgas");
    assert_eq!(near(10u128.pow(20)), "0.0001 NEAR");
    assert_eq!(near(10u128.pow(20) - 1), "99999999999999999999 yocto");
    assert_eq!(near(10u128.pow(24)), "1.0000 NEAR");
    let mut r = sample(false).receipts[3].clone();
    assert_eq!(receipt_label(&r), "carol.near <- (no actions)");
    r.actions = vec!["Transfer".into()];
    r.deposit = Balance::from_yoctonear(5);
    assert_eq!(receipt_label(&r), "carol.near <- transfer 5 yocto");
    r.actions = vec!["FunctionCall".into(), "FunctionCall".into()];
    r.method_names = vec!["f".into(), "g".into()];
    // Carol is missing: the batch failed at its first call.
    assert_eq!(receipt_label(&r), "carol.near::f,g (failed at f)");
    r.actions.insert(0, "DeployContract".into());
    r.actions.push("Transfer".into());
    assert_eq!(receipt_label(&r), "carol.near <- DeployContract + f + g + Transfer");
    let refund = &sample(false).receipts[4];
    assert_eq!(receipt_label(refund), "alice.near <- (no actions) (refund)");
}

#[test]
fn columns_by_cost_name() {
    for (cost, col) in [
        ("NEW_ACTION_RECEIPT", SEND),
        ("FUNCTION_CALL_BYTE", SEND),
        ("TRANSFER", SEND),
        ("WASM_INSTRUCTION", 0),
        ("CONTRACT_LOADING_BYTES", 1),
        ("STORAGE_WRITE_BASE", 2),
        ("TOUCHING_TRIE_NODE", 2),
        ("READ_CACHED_TRIE_NODE", 2),
        ("BASE", 3),
        ("LOG_BYTE", 3),
        ("PROMISE_RETURN", 3),
    ] {
        assert_eq!(column(cost), col, "{cost}");
    }
}

#[test]
fn gas_table_and_display() {
    let mut o = sample(false);
    let tg = |x: u64| x * 100_000_000_000; // tenths of a Tgas
    o.receipts[0].gas_profile = [
        ("WASM_INSTRUCTION", tg(4)),
        ("CONTRACT_LOADING_BYTES", tg(3)),
        ("STORAGE_WRITE_BASE", tg(2)),
        ("TOUCHING_TRIE_NODE", tg(1)),
        ("BASE", tg(1)),
        ("NEW_ACTION_RECEIPT", tg(4)),
    ]
    .map(|(c, g)| (c.to_string(), g))
    .into();
    let want = "\
gas (Tgas)                             wasm    load storage    host    send    exec   other   total
conversion alice.near                     -       -       -       -   0.100       -       -   0.100
#2 app.near::run                      0.400   0.300   0.300   0.100   0.400   0.500       -   2.000
#3 bob.near <- transfer 1.0000 NEAR       -       -       -       -       -       -   0.223   0.223
#4 app.near::cb                           -       -       -       -       -       -   1.000   1.000
#5 carol.near <- (no actions)             -       -       -       -       -       -       -       -
total 3.32 Tgas (3323182562500 gas)
";
    assert_eq!(o.gas_table(false, false), want);
    // Refunds that report gas get one row.
    o.receipts[4].gas_burnt = Gas::from_gas(tg(2));
    let (t, cells) = (o.gas_table(false, false), " ".repeat(14) + &"       -".repeat(7));
    let want =
        format!("\nrefunds (not charged){cells}   0.200\ntotal 3.52 Tgas (3523182562500 gas)\n");
    assert!(t.ends_with(&want), "{t}");
    // Everything explained: no `other` column.
    o.receipts[1].prepaid_exec_fees = o.receipts[1].gas_burnt;
    o.receipts[2].gas_profile = [("WASM_INSTRUCTION".to_string(), tg(10))].into();
    o.receipts.retain(|r| !r.is_refund());
    o.transaction = None;
    let want = "\
gas (Tgas)                             wasm    load storage    host    send    exec   total
#2 app.near::run                      0.400   0.300   0.300   0.100   0.400   0.500   2.000
#3 bob.near <- transfer 1.0000 NEAR       -       -       -       -       -   0.223   0.223
#4 app.near::cb                       1.000       -       -       -       -       -   1.000
#5 carol.near <- (no actions)             -       -       -       -       -       -       -
total 3.22 Tgas (3223182562500 gas)
";
    assert_eq!(o.gas_table(false, false), want);
    // Short labels are padded to the header's width.
    o.receipts.truncate(1);
    o.receipts[0].receiver_id = "ab".parse().unwrap();
    o.receipts[0].method_names = vec!["f".into()];
    let t = o.gas_table(false, false);
    assert!(t.starts_with("gas (Tgas)    wasm ") && t.contains("\n#2 ab::f     0.400 "), "{t}");

    let o = sample(false);
    assert_eq!(o.to_string(), format!("{}\n{}", o.tree(false, false), o.gas_table(false, false)));
    assert_eq!(format!("{o:#}"), format!("{}\n{}", o.tree(true, false), o.gas_table(false, false)));
    // Nothing ran yet: no gas to show.
    let mut o = o;
    o.transaction = None;
    o.receipts.clear();
    assert_eq!(o.to_string(), o.tree(false, false));
}

#[test]
fn gas_table_colors() {
    let mut o = sample(false);
    o.receipts[4].gas_burnt = Gas::from_gas(200_000_000_000);
    let plain = o.gas_table(false, false);
    let lines: Vec<&str> = plain.lines().collect();
    // The label column; a failed receipt's label is red.
    let width = lines[0].len() - 8 * 8;
    let mut want: Vec<String> = lines
        .iter()
        .map(|l| {
            let (label, cells) = l.split_at(width.min(l.len()));
            let failed = label.starts_with("#3 ") || label.starts_with("#5 ");
            let label = if failed { format!("\x1b[31m{label}\x1b[0m") } else { label.into() };
            label + &cells.replace("       -", "\x1b[2m       -\x1b[0m")
        })
        .collect();
    let n = want.len();
    assert!(lines[n - 2].starts_with("refunds"));
    want[n - 2] = format!("\x1b[2m{}\x1b[0m", lines[n - 2]);
    want[n - 1] = format!("\x1b[1m{}\x1b[0m", lines[n - 1]);
    assert_eq!(o.gas_table(false, true), want.join("\n") + "\n");
}

#[test]
fn the_trace_is_the_colored_rendering() {
    let o = sample(false);
    let colored = format!("{}\n{}", o.tree(false, true), o.gas_table(false, true));
    assert_eq!(o.render(false, true), colored);
    let want = if color() { colored } else { o.to_string() };
    assert_eq!(trace(&o), format!("\n\n{want}"));
}

#[test]
fn tree_text() {
    let s = |n| short(&id(n));
    let want = format!(
        "tx {} signed by alice.near at #1 (conversion 0.10 Tgas) -> SUCCESS\n\
         └─ #2 app.near::run  attached 10.00 Tgas, burnt 2.00 Tgas (1.50 + 0.50 exec fees), headroom 8.50 Tgas  ok (returns a promise)\n\
         \x20  ├─ #3 bob.near <- transfer 1.0000 NEAR  burnt 0.22 Tgas  FAILED: account bob.near does not exist\n\
         \x20  └─ #4 app.near::cb [callback of bob.near <- transfer 1.0000 NEAR & yield/resume]  attached 5.00 Tgas, burnt 1.00 Tgas (1.00 + 0.00 exec fees), headroom 4.00 Tgas  ok -> true\n\
         \x20     ├─ #5 carol.near <- (no actions)  burnt 0.00 Tgas  FAILED: account carol.near does not exist  <-- UNHANDLED\n\
         \x20     └─ {} (not executed yet)\n\
         (1 refund receipt(s) not shown)\n",
        s(0),
        s(5)
    );
    assert_eq!(sample(true).tree(false, false), want);
    // A pending receipt is named, with where it waits.
    let mut o = sample(true);
    o.pending = vec![crate::PendingReceipt {
        receipt_id: id(5),
        tx: id(0),
        predecessor_id: "app.near".parse().unwrap(),
        receiver_id: "dave.near".parse().unwrap(),
        method_names: vec!["pay".into()],
        deposit: Balance::from_near(1),
        state: crate::PendingState::Held { blocks_left: 2 },
    }];
    let line = format!("└─ {} dave.near::pay (not executed yet: held, 2 more blocks)\n", s(5));
    assert!(o.tree(false, false).contains(&line), "{}", o.tree(false, false));
    // No refunds, no note.
    let mut o = sample(false);
    o.receipts.pop();
    o.receipts[0].receipt_ids.pop();
    let t = o.tree(false, false);
    assert!(!t.contains("refund"), "{t}");
}

#[test]
fn tree_logs_and_refunds() {
    let mut o = sample(false);
    o.receipts[0].logs = vec!["a\nb".into()];
    o.receipts[3].logs = vec!["x".into()];
    let want = format!(
        "tx {} signed by alice.near at #1 (conversion 0.10 Tgas) -> SUCCESS\n\
         └─ #2 app.near::run  attached 10.00 Tgas, burnt 2.00 Tgas (1.50 + 0.50 exec fees), headroom 8.50 Tgas  ok (returns a promise)\n\
         \x20  │  log: a\\nb\n\
         \x20  ├─ #3 bob.near <- transfer 1.0000 NEAR  burnt 0.22 Tgas  FAILED: account bob.near does not exist\n\
         \x20  ├─ #4 app.near::cb [callback of bob.near <- transfer 1.0000 NEAR & yield/resume]  attached 5.00 Tgas, burnt 1.00 Tgas (1.00 + 0.00 exec fees), headroom 4.00 Tgas  ok -> true\n\
         \x20  │  └─ #5 carol.near <- (no actions)  burnt 0.00 Tgas  FAILED: account carol.near does not exist  <-- UNHANDLED\n\
         \x20  │        log (rolled back): x\n\
         \x20  └─ #1 alice.near <- (no actions) (refund)  burnt 0.00 Tgas  FAILED: account alice.near does not exist\n",
        short(&id(0))
    );
    assert_eq!(o.tree(true, false), want);
    // By default the refund is only counted, unless it is unhandled.
    let t = o.tree(false, false);
    assert!(t.ends_with("        log (rolled back): x\n(1 refund receipt(s) not shown)\n"), "{t}");
    assert!(!t.contains("(refund)"), "{t}");
    o.receipts[4].unhandled = true;
    let t = o.tree(false, false);
    let lost =
        "(refund)  burnt 0.00 Tgas  FAILED: account alice.near does not exist  <-- UNHANDLED";
    assert!(t.ends_with(&format!("{lost}\n")), "{t}");
}

/// An ok line shows the value, cut unless `full`.
#[test]
fn tree_values() {
    let mut o = sample(false);
    o.receipts[2].status = ExecutionStatus::SuccessValue(vec![b'a'; 41]);
    let cut = format!("  ok -> {}… (+1 chars)\n", "a".repeat(40));
    assert!(o.tree(false, false).contains(&cut), "{o}");
    let full = format!("  ok -> {}\n", "a".repeat(41));
    assert!(o.tree(true, false).contains(&full), "{o:#}");
}

#[test]
fn tree_colors() {
    let (r, y, d, x) = ("\x1b[31m", "\x1b[33m", "\x1b[2m", "\x1b[0m");
    let mut o = sample(false);
    o.receipts[0].logs = vec!["a".into()];
    let want = format!(
        "tx {} signed by alice.near at #1 (conversion 0.10 Tgas) -> SUCCESS\n\
         └─ #2 app.near::run  attached 10.00 Tgas, burnt 2.00 Tgas (1.50 + 0.50 exec fees), headroom 8.50 Tgas  ok (returns a promise)\n\
         \x20  │  {y}log: a{x}\n\
         \x20  ├─ #3 bob.near <- transfer 1.0000 NEAR  burnt 0.22 Tgas  {r}FAILED: account bob.near does not exist{x}\n\
         \x20  ├─ #4 app.near::cb [callback of bob.near <- transfer 1.0000 NEAR & yield/resume]  attached 5.00 Tgas, burnt 1.00 Tgas (1.00 + 0.00 exec fees), headroom 4.00 Tgas  ok -> true\n\
         \x20  │  └─ #5 carol.near <- (no actions)  burnt 0.00 Tgas  {r}FAILED: account carol.near does not exist  <-- UNHANDLED{x}\n\
         {d}   └─ #1 alice.near <- (no actions) (refund)  burnt 0.00 Tgas  {r}FAILED: account alice.near does not exist{x}{x}\n",
        short(&id(0))
    );
    assert_eq!(o.tree(true, true), want);
    let t = o.tree(false, true);
    assert!(t.ends_with(&format!("\n{d}(1 refund receipt(s) not shown){x}\n")), "{t}");
    o.status = FinalExecutionStatus::Failure(TxExecutionError::InvalidTxError(
        InvalidTxError::InvalidSignature,
    ));
    let t = o.tree(false, true);
    let invalid = "invalid transaction: Transaction is not signed with the given public key";
    let want = format!(" -> {r}FAILURE: {invalid}{x}\n");
    assert!(t.contains(&want), "{t}");
}

#[test]
fn log_lines_cut_long_logs_unless_full() {
    let logs = |n: usize, len: usize| -> Vec<String> { vec!["é".repeat(len); n] };
    assert_eq!(log_lines(&logs(10, 200), false), logs(10, 200));
    let cut = log_lines(&logs(11, 201), false);
    assert_eq!(cut.len(), 11);
    assert_eq!(cut[0], format!("{}… (+1 chars)", "é".repeat(200)));
    assert_eq!(cut[10], "… 1 more log(s)");
    assert_eq!(log_lines(&logs(11, 201), true), logs(11, 201));
    assert_eq!(log_lines(&["a\tb\u{7}é".into()], false), ["a\\tb\\u{7}é"]);
}

#[test]
fn status_labels() {
    let mut o = sample(false);
    for pending in [FinalExecutionStatus::NotStarted, FinalExecutionStatus::Started] {
        o.status = pending;
        assert_eq!(status_label(&o), "PENDING");
    }
    let e = TxExecutionError::ActionError(ActionError {
        index: None,
        kind: ActionErrorKind::AccountDoesNotExist { account_id: "x.near".parse().unwrap() },
    });
    o.status = FinalExecutionStatus::Failure(e);
    assert_eq!(status_label(&o), "FAILURE: account x.near does not exist");
}

/// A near-sdk panic, whose message spans two lines, stays on the header's
/// line and on its receipt's.
#[test]
fn failures_render_on_one_line() {
    let m = "Smart contract panicked: panicked at src/lib.rs:1:1:\nboom";
    let e = TxExecutionError::ActionError(ActionError {
        index: Some(0),
        kind: ActionErrorKind::FunctionCallError(FunctionCallError::ExecutionError(m.into())),
    });
    let mut o = sample(false);
    o.status = FinalExecutionStatus::Failure(e.clone());
    o.receipts[0].status = ExecutionStatus::Failure(e);
    let line = "panicked: panicked at src/lib.rs:1:1:\\nboom";
    assert_eq!(status_label(&o), format!("FAILURE: {line}"));
    let t = o.tree(false, false);
    assert!(t.contains(&format!("  FAILED: {line}\n")), "{t}");
}

#[test]
fn gas_summaries() {
    let o = sample(false);
    assert_eq!(gas_summary(&o.receipts[1]), "burnt 0.22 Tgas");
    let mut oog = o.receipts[0].clone();
    oog.attached_gas = Gas::from_teragas(3);
    oog.gas_burnt = Gas::from_gas(3_650_000_000_000);
    oog.prepaid_exec_fees = Gas::from_gas(650_000_000_000);
    oog.status = missing("x.near");
    assert_eq!(gas_summary(&oog), "attached 3.00 Tgas, burnt 3.65 Tgas (3.00 + 0.65 exec fees)");
    assert_eq!(
        gas_summary(&o.receipts[2]),
        "attached 5.00 Tgas, burnt 1.00 Tgas (1.00 + 0.00 exec fees), headroom 4.00 Tgas"
    );
}

/// Runs of four or more like items keep their first and last; the middle
/// is one `Many`. `None` keys and `full` never collapse.
#[test]
fn runs_collapse_the_middle_of_like_items() {
    let shape = |items: &[u8], full| -> String {
        let key = |x: &u8| (*x != 0).then_some(*x);
        runs(items, key, full)
            .iter()
            .map(|r| match r {
                Run::One(x) => x.to_string(),
                Run::Many(xs) => format!("{}x{}", xs.len(), xs[0]),
            })
            .collect::<Vec<_>>()
            .join(" ")
    };
    assert_eq!(shape(&[1, 1, 1, 2], false), "1 1 1 2");
    assert_eq!(shape(&[1, 1, 1, 1, 2, 2, 2, 2, 2], false), "1 2x1 1 2 3x2 2");
    assert_eq!(shape(&[0, 0, 0, 0], false), "0 0 0 0");
    assert_eq!(shape(&[1, 1, 1, 1], true), "1 1 1 1");
}

/// A chain of like receipts collapses in the tree and the gas table, whose
/// middle row sums the rows it stands for; `{:#}` shows them all.
#[test]
fn like_receipts_collapse() {
    let mut o = sample(false);
    o.receipts.truncate(1);
    o.receipts[0].receipt_ids = vec![id(10)];
    for n in 10..16u8 {
        let mut r = o.receipts[0].clone();
        r.receipt_id = id(n);
        r.block_height = u64::from(n);
        r.receipt_ids = if n < 15 { vec![id(n + 1)] } else { vec![] };
        r.status = ExecutionStatus::SuccessReceiptId(id(n + 1));
        o.receipts.push(r);
    }
    let t = o.tree(false, false);
    assert_eq!(t.lines().count(), 4, "{t}");
    assert!(t.contains("\n   └─ … 5 more app.near::run: ok (returns a promise)\n"), "{t}");
    assert_eq!(o.tree(true, false).lines().count(), 8);
    let table = o.gas_table(false, false);
    assert!(table.contains("\n… 5 more app.near::run "), "{table}");
    assert!(table.ends_with("total 14.10 Tgas (14100000000000 gas)\n"), "{table}");
    assert_eq!(table.lines().count(), 6, "{table}");
    assert_eq!(o.gas_table(true, false).lines().count(), 10);
}

/// A long chain whose receipts alternate, so no run collapses, still fits
/// a budget: its ends and its first failure stay, each gap says how many
/// receipts it hides and how many failed; `{:#}` shows them all.
#[test]
fn a_long_tree_keeps_its_ends_and_first_failure() {
    let mut o = sample(false);
    o.receipts.truncate(1);
    o.receipts[0].receipt_ids = vec![id(10)];
    // Its first receipt was delivered from another account (`Tx::predecessor`).
    o.receipts[0].delivered = true;
    o.receipts[0].predecessor_id = "evil.near".parse().unwrap();
    for n in 10..210u8 {
        let mut r = ro(n, if n % 2 == 0 { "a.near" } else { "b.near" });
        r.actions = vec!["FunctionCall".into()];
        r.method_names = vec!["run".into()];
        r.block_height = u64::from(n);
        r.gas_burnt = Gas::from_gas(1_000_000_000_000);
        r.receipt_ids = if n < 209 { vec![id(n + 1)] } else { vec![] };
        if n == 120 || n == 150 {
            r.status = missing("gone.near");
        }
        o.receipts.push(r);
    }
    let t = o.tree(false, false);
    assert!(t.lines().count() <= MAX_TREE_LINES + 1, "{} lines:\n{t}", t.lines().count());
    assert!(t.contains("#120 a.near::run "), "{t}");
    assert!(!t.contains("#150 "), "{t}");
    assert!(t.contains("#10 a.near::run ") && t.contains("#209 b.near::run "), "{t}");
    assert!(t.contains(" failed; {:#} shows all\n"), "{t}");
    assert_eq!(o.tree(true, false).lines().count(), 202);
    let table = o.gas_table(false, false);
    assert!(table.lines().count() <= MAX_TABLE_ROWS + 4, "{table}");
    assert!(table.contains("\n#120 a.near::run "), "{table}");
    let delivered = "#2 app.near::run [delivered from evil.near] ";
    assert!(t.contains(delivered) && table.contains(&format!("\n{delivered}")), "{t}{table}");
    assert!(table.contains(" more receipt(s), 1 failed "), "{table}");
    assert!(table.contains("total 202.10 Tgas"), "{table}");
    // One gap hides receipts that succeeded, dimmed; the other hides #150.
    let gaps: Vec<&str> = t.lines().filter(|l| l.contains("shows all")).collect();
    assert_eq!(gaps.len(), 2, "{t}");
    assert!(gaps[0].ends_with(" more receipt(s); {:#} shows all"), "{t}");
    assert!(gaps[1].ends_with(" more receipt(s), 1 failed; {:#} shows all"), "{t}");
    let colored = o.tree(false, true);
    let gaps: Vec<&str> = colored.lines().filter(|l| l.contains("shows all")).collect();
    assert!(gaps[0].contains(&format!("\x1b[{DIM}m… ")), "{colored}");
    assert!(gaps[1].contains(&format!("\x1b[{RED}m… ")), "{colored}");
}

/// No failure prints nearcore's `AccountId("x")`, or a variant's name twice.
#[test]
fn failures_name_accounts_plainly() {
    let id = |s: &str| -> AccountId { s.parse().unwrap() };
    let kind = ActionErrorKind::CreateAccountNotAllowed {
        account_id: id("x.bob.near"),
        predecessor_id: id("alice.near"),
    };
    let text = Failure::Action(kind).to_string();
    assert!(text.starts_with("CreateAccountNotAllowed: "), "{text}");
    assert!(text.contains(" x.bob.near ") && text.contains(" alice.near"), "{text}");
    assert!(!text.contains("AccountId("), "{text}");
    let trap = FunctionCallError::WasmUnknownError;
    let text = Failure::Action(ActionErrorKind::FunctionCallError(trap)).to_string();
    assert_eq!(text.matches("WasmUnknownError").count(), 1, "{text}");
    assert_eq!(plain_ids(r#"a AccountId("b") c AccountId("d"#), r#"a b c AccountId("d"#);
}

/// `budget` keeps the first failure, then takes from each end in turn
/// until an entry doesn't fit, a line kept for each of two gaps; an
/// entry the other end or the failure holds costs nothing.
#[test]
fn budget_takes_from_both_ends_in_turn() {
    // Entries of `lines[i]` lines; `first` marks the first failure.
    let keep = |lines: &[usize], first: Option<usize>, max: usize| -> String {
        let entries: Vec<Entry<usize>> = (lines.iter().enumerate())
            .map(|(i, n)| Entry {
                text: String::new(),
                of: *n,
                receipts: 1,
                failed: 0,
                first: first == Some(i),
            })
            .collect();
        let keep = budget(&entries, |e| e.of, max, false);
        keep.iter().map(|k| if *k { 'k' } else { '.' }).collect()
    };
    let ones = [1; 10];
    assert_eq!(keep(&ones, None, 10), "kkkkkkkkkk");
    assert_eq!(keep(&ones, None, 9), "kkkk...kkk");
    assert_eq!(keep(&ones, None, 7), "kkk.....kk");
    assert_eq!(keep(&ones, Some(5), 7), "kk...k..kk");
    // An end stops at the first entry that doesn't fit, not the next.
    assert_eq!(keep(&[1, 5, 1, 1, 1, 1, 1, 1], None, 8), "k..kkkkk");
    // An exact fit is taken.
    assert_eq!(keep(&[2, 2, 2, 2], None, 6), "k..k");
    // The failure, at the head, costs nothing when the head reaches it.
    assert_eq!(keep(&[3, 1, 1, 1, 1, 1], Some(0), 7), "kk...k");
    // The tail passes the failure at no cost, then stops.
    assert_eq!(keep(&[1, 1, 1, 1, 9], Some(4), 12), "k...k");
    let entries = [Entry { text: String::new(), of: 9, receipts: 1, failed: 0, first: false }];
    assert_eq!(budget(&entries, |e| e.of, 1, true), [true]);
}

/// A missing method lists at most 30 exports, and says when there are none.
#[test]
fn missing_method_caps_the_list() {
    let id: AccountId = "c.near".parse().unwrap();
    let names = |n: usize| (0..n).map(|i| format!("m{i:02}")).collect::<Vec<_>>();
    let none = missing_method(&id, "x", &[]);
    assert_eq!(none, "MethodNotFound: c.near has no method x; it exports none");
    let thirty = missing_method(&id, "x", &names(30));
    assert!(thirty.ends_with(", m29") && !thirty.contains("more"), "{thirty}");
    let more = missing_method(&id, "x", &names(31));
    assert!(more.ends_with(", m29, … 1 more"), "{more}");
}
