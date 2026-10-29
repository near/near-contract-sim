//! Human-readable outcomes: `TxOutcome`'s `Display` (the receipt tree, then
//! a gas table), and the trace the asserting finishers append to their
//! panics, which is the same rendering in color (see [`color`]).

use crate::effects::PendingReceipt;
use crate::outcome::{Failure, ReceiptOutcome, TxOutcome};
use near_parameters::ActionCosts;
use near_primitives::hash::CryptoHash;
use near_primitives::types::{AccountId, Gas};
use near_primitives::views::FinalExecutionStatus;
use std::collections::HashMap;
use std::fmt;
use std::io::IsTerminal;
use strum::IntoEnumIterator;

impl fmt::Display for TxOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.render(f.alternate(), false))
    }
}

/// What `TxOutcome`'s panicking methods (`assert_success`, `assert_failure`,
/// `json`, `expect_receipt`, `assert_headroom`) append to their message, and
/// a built-in finding's `Display`.
pub(crate) fn trace(o: &TxOutcome) -> String {
    format!("\n\n{}", o.render(false, color()))
}

const RED: &str = "31";
const YELLOW: &str = "33";
const DIM: &str = "2";
const BOLD: &str = "1";

/// `s` in the ANSI style `sgr` if `on`.
fn paint(on: bool, sgr: &str, s: &str) -> String {
    if on { format!("\x1b[{sgr}m{s}\x1b[0m") } else { s.to_string() }
}

/// Whether failure renderings are colored: never under `NO_COLOR` or in an
/// invariant runner's closure (a panic there is a violation's plain
/// message), always under `CLICOLOR_FORCE` (other than `0`) or nextest
/// (which captures test output, then shows it in color), otherwise if
/// stderr is a terminal.
pub(crate) fn color() -> bool {
    let var = |k| std::env::var_os(k).filter(|v| !v.is_empty());
    var("NO_COLOR").is_none()
        && !crate::findings::guarding()
        && (var("CLICOLOR_FORCE").is_some_and(|v| v != "0")
            || var("NEXTEST").is_some()
            || std::io::stderr().is_terminal())
}

const COLUMNS: [&str; 7] = ["wasm", "load", "storage", "host", "send", "exec", "other"];
const SEND: usize = 4;
const EXEC: usize = 5;
const OTHER: usize = 6;
const REFUNDS: &str = "refunds (not charged)";

/// Column of a gas-profile entry. nearcore's `ACTION_COST` entries are the
/// send fees of the receipts the call created; the rest (`WASM_HOST_COST`)
/// split into wasm execution, contract loading, storage (including trie
/// nodes) and every other host function.
fn column(cost: &str) -> usize {
    if ActionCosts::iter().any(|a| format!("{a:?}").eq_ignore_ascii_case(cost)) {
        SEND
    } else if cost == "WASM_INSTRUCTION" {
        0
    } else if cost.starts_with("CONTRACT_LOADING_") {
        1
    } else if cost.starts_with("STORAGE_") || cost.ends_with("_TRIE_NODE") {
        2
    } else {
        3
    }
}

/// A receipt's gas by column. The profile leaves out the receipt's own exec
/// fees, which are [`ReceiptOutcome::prepaid_exec_fees`]; `other` is
/// whatever is still unexplained, so the columns add up to `gas_burnt`.
fn columns(r: &ReceiptOutcome) -> [u64; 7] {
    let mut cols = [0u64; 7];
    for (cost, gas) in &r.gas_profile {
        cols[column(cost)] += gas;
    }
    cols[EXEC] = r.prepaid_exec_fees.as_gas();
    cols[OTHER] = r.gas_burnt.as_gas().saturating_sub(cols.iter().sum());
    cols
}

impl TxOutcome {
    /// A row of [`columns`] per receipt, one for all the refunds, then the
    /// total. Unless `full`, a run of rows for the same receipt label
    /// collapses into its first, one row summing the middle, and its last;
    /// past `MAX_TABLE_ROWS`, a row sums each gap `budget` leaves.
    fn gas_table(&self, full: bool, color: bool) -> String {
        let mut rows: Vec<(String, [u64; 7], u64, bool)> = vec![];
        if let Some(t) = &self.transaction {
            let mut cols = [0; 7];
            cols[SEND] = t.gas_burnt.as_gas();
            let failed = t.failure_text().is_some();
            rows.push((
                format!("conversion {}", t.receiver_id),
                cols,
                t.gas_burnt.as_gas(),
                failed,
            ));
        }
        let calls: Vec<&ReceiptOutcome> = self.receipts.iter().filter(|r| !r.is_refund()).collect();
        let first = first_failure(self);
        let label = |r: &ReceiptOutcome| receipt_label(r) + &delivered_from(r).unwrap_or_default();
        let key = |r: &&ReceiptOutcome| Some((label(r), r.is_success()));
        let sum = |rs: &[&ReceiptOutcome]| {
            let mut cols = [0u64; 7];
            for r in rs {
                for (c, g) in cols.iter_mut().zip(columns(r)) {
                    *c += g;
                }
            }
            (cols, rs.iter().map(|r| r.gas_burnt.as_gas()).sum::<u64>())
        };
        let entries: Vec<Entry<&[&ReceiptOutcome]>> = runs(&calls, key, full)
            .into_iter()
            .map(|run| {
                let (rs, label) = match run {
                    Run::One(r) => {
                        (std::slice::from_ref(r), format!("#{} {}", r.block_height, label(r)))
                    }
                    Run::Many(rs) => {
                        let failed = if rs[0].is_success() { "" } else { " (failed)" };
                        (rs, format!("… {} more {}{failed}", rs.len(), label(rs[0])))
                    }
                };
                Entry {
                    text: label,
                    of: rs,
                    receipts: rs.len(),
                    failed: rs.iter().filter(|r| !r.is_success()).count(),
                    first: rs.iter().any(|r| Some(r.receipt_id) == first),
                }
            })
            .collect();
        let keep = budget(&entries, |_| 1, MAX_TABLE_ROWS, full);
        for shown in gaps(&entries, &keep) {
            let (label, rs): (String, Vec<&ReceiptOutcome>) = match shown {
                Shown::Item(e) => (e.text.clone(), e.of.to_vec()),
                Shown::Gap(es) => {
                    (gap_label(es), es.iter().flat_map(|e| e.of.iter().copied()).collect())
                }
            };
            let (cols, total) = sum(&rs);
            let failed = rs.iter().any(|r| !r.is_success());
            rows.push((label, cols, total, failed));
        }
        let refunds: u64 =
            self.receipts.iter().filter(|r| r.is_refund()).map(|r| r.gas_burnt.as_gas()).sum();
        if refunds > 0 {
            rows.push((REFUNDS.into(), [0; 7], refunds, false));
        }
        let shown = if rows.iter().any(|r| r.1[OTHER] > 0) { 7 } else { 6 };
        let width = rows.iter().map(|r| r.0.chars().count()).fold(10, usize::max);
        let cell = |g: u64, on| match g {
            0 => paint(on, DIM, &format!(" {:>7}", "-")),
            g => format!(" {:>7.3}", g as f64 / 1e12),
        };
        let mut out = format!("{:width$}", "gas (Tgas)");
        for c in &COLUMNS[..shown] {
            out.push_str(&format!(" {c:>7}"));
        }
        out.push_str(&format!(" {:>7}\n", "total"));
        for (label, cols, total, failed) in &rows {
            // Dimmed whole: a dim cell's reset would end the row's dim early.
            let refunds = label == REFUNDS;
            let mut row = paint(color && *failed, RED, &format!("{label:width$}"));
            for g in cols[..shown].iter().chain([total]) {
                row.push_str(&cell(*g, color && !refunds));
            }
            out.push_str(&paint(color && refunds, DIM, &row));
            out.push('\n');
        }
        let total = self.gas_burnt();
        let line = format!("total {} ({} gas)", tgas(total), total.as_gas());
        out.push_str(&paint(color, BOLD, &line));
        out.push('\n');
        out
    }

    /// The `Display` rendering, colored if `color`.
    pub(crate) fn render(&self, full: bool, color: bool) -> String {
        let mut s = self.tree(full, color);
        if self.transaction.is_some() {
            s.push('\n');
            s.push_str(&self.gas_table(full, color));
        }
        s
    }

    /// The receipt tree: executor, method, attached / burnt gas, status,
    /// logs, and which callbacks receive whose results. Unless `full`,
    /// handled refund receipts are left out (their count is shown), long
    /// logs are cut, like receipts in a row collapse, and past
    /// `MAX_TREE_LINES` a line stands for each gap `budget` leaves.
    fn tree(&self, full: bool, color: bool) -> String {
        let Some(tx) = &self.transaction else {
            return format!("tx {} -> QUEUED (not in a block yet)\n", short(&self.hash));
        };
        let mut t = Tree {
            by_id: self.receipts.iter().map(|r| (r.receipt_id, r)).collect(),
            pending: self.pending.iter().map(|p| (p.receipt_id, p)).collect(),
            producer: HashMap::new(),
            full,
            color,
            first: first_failure(self),
            units: vec![],
        };
        for r in &self.receipts {
            for d in &r.output_data_receivers {
                t.producer.insert(d.data_id, r);
            }
        }
        let refunds = self.receipts.iter().filter(|r| r.is_refund() && !r.unhandled).count();
        let status = paint(color && self.is_failure(), RED, &status_label(self));
        let mut out = format!(
            "tx {} signed by {} at #{} (conversion {}) -> {}\n",
            short(&self.hash),
            tx.receiver_id,
            tx.block_height,
            tgas(tx.gas_burnt),
            status
        );
        let visible: Vec<CryptoHash> =
            tx.receipt_ids.iter().copied().filter(|id| t.shown(id)).collect();
        for (i, id) in visible.iter().enumerate() {
            t.render(*id, "", i + 1 == visible.len());
        }
        // Each entry is `of` its lead: the tree drawn before it.
        let entries: Vec<Entry<&String>> = runs(&t.units, |u| u.key.clone(), full)
            .into_iter()
            .map(|run| match run {
                Run::One(u) => Entry {
                    text: u.text.clone(),
                    of: &u.lead,
                    receipts: 1,
                    failed: usize::from(u.failed),
                    first: u.first,
                },
                Run::Many(us) => {
                    let what = us[0].key.as_deref().unwrap_or_default();
                    let line = format!("… {} more {what}", us.len());
                    let style = if us[0].failed { RED } else { DIM };
                    Entry {
                        text: format!("{}{}\n", us[0].lead, paint(color, style, &line)),
                        of: &us[0].lead,
                        receipts: us.len(),
                        failed: if us[0].failed { us.len() } else { 0 },
                        first: us.iter().any(|u| u.first),
                    }
                }
            })
            .collect();
        let keep = budget(&entries, |e| e.text.lines().count(), MAX_TREE_LINES, full);
        for shown in gaps(&entries, &keep) {
            match shown {
                Shown::Item(e) => out.push_str(&e.text),
                Shown::Gap(es) => {
                    let style = if es.iter().any(|e| e.failed > 0) { RED } else { DIM };
                    let line = paint(color, style, &format!("{}; {{:#}} shows all", gap_label(es)));
                    out.push_str(&format!("{}{line}\n", es[0].of));
                }
            }
        }
        if refunds > 0 && !full {
            let note = format!("({refunds} refund receipt(s) not shown)");
            out.push_str(&paint(color, DIM, &note));
            out.push('\n');
        }
        out
    }
}

/// State of one receipt-tree rendering.
struct Tree<'a> {
    by_id: HashMap<CryptoHash, &'a ReceiptOutcome>,
    /// The receipts that haven't run, where each waits.
    pending: HashMap<CryptoHash, &'a PendingReceipt>,
    /// data_id -> receipt producing it, to label callbacks.
    producer: HashMap<CryptoHash, &'a ReceiptOutcome>,
    /// Refunds shown, logs not cut and runs not collapsed.
    full: bool,
    color: bool,
    /// The receipt that failed first, which a long tree keeps.
    first: Option<CryptoHash>,
    units: Vec<Unit>,
}

/// A receipt's lines in the tree: its own, then its logs.
struct Unit {
    /// The tree drawn before it: indentation and branch.
    lead: String,
    /// What a run of like receipts shares: label, markers and status, not
    /// gas or value. `None` never collapses.
    key: Option<String>,
    text: String,
    failed: bool,
    /// Whether it's the transaction's first failure, which stays shown.
    first: bool,
}

/// A slice cut into runs: an item alone, or `MIN_RUN` or more like ones in a
/// row, whose middle collapses (the first and last stay `One`).
enum Run<'a, T> {
    One(&'a T),
    Many(&'a [T]),
}

/// The fewest like items in a row that collapse.
const MIN_RUN: usize = 4;

/// `items` as runs of equal `key`s; all `One` if `full`. A `None` key is
/// never like another.
fn runs<'a, T, K: PartialEq>(
    items: &'a [T],
    key: impl Fn(&T) -> Option<K>,
    full: bool,
) -> Vec<Run<'a, T>> {
    let mut out = vec![];
    for run in items.chunk_by(|a, b| key(a).is_some_and(|k| Some(k) == key(b))) {
        match run {
            [first, middle @ .., last] if !full && run.len() >= MIN_RUN => {
                out.extend([Run::One(first), Run::Many(middle), Run::One(last)]);
            }
            _ => out.extend(run.iter().map(Run::One)),
        }
    }
    out
}

/// The most lines a tree shows, unless `full`.
const MAX_TREE_LINES: usize = 50;
/// The most receipt rows a gas table shows, unless `full`.
const MAX_TABLE_ROWS: usize = 30;

/// A run as rendered: its `text`, and what a gap that hides it needs.
struct Entry<T> {
    text: String,
    of: T,
    receipts: usize,
    failed: usize,
    /// Whether it holds the transaction's first failure.
    first: bool,
}

/// Which `entries` fit in `max` lines (`lines` counts an entry's): all of
/// them if they do, else the first failure, then from each end in turn,
/// keeping a line for each gap. All if `full`.
fn budget<T>(
    entries: &[Entry<T>],
    lines: impl Fn(&Entry<T>) -> usize,
    max: usize,
    full: bool,
) -> Vec<bool> {
    let n = entries.len();
    if full || entries.iter().map(&lines).sum::<usize>() <= max {
        return vec![true; n];
    }
    let mut keep = vec![false; n];
    // Kept are a head, the first failure and a tail: two gaps at most.
    let mut room = max.saturating_sub(2);
    if let Some(i) = entries.iter().position(|e| e.first) {
        keep[i] = true;
        room = room.saturating_sub(lines(&entries[i]));
    }
    // The head and the tail take turns, each until an entry doesn't fit.
    // Where they meet, the other's entries are kept and cost nothing.
    let mut taking = [true, true];
    for (side, i) in (0..n).flat_map(|k| [(0, k), (1, n - 1 - k)]) {
        if !taking[side] {
            continue;
        }
        let need = if keep[i] { 0 } else { lines(&entries[i]) };
        if need <= room {
            room -= need;
            keep[i] = true;
        } else {
            taking[side] = false;
        }
    }
    keep
}

/// An entry `budget` kept, or a run of ones it left out.
enum Shown<'a, T> {
    Item(&'a T),
    Gap(&'a [T]),
}

/// `items` with each run of unkept ones as one `Gap`.
fn gaps<'a, T>(items: &'a [T], keep: &[bool]) -> Vec<Shown<'a, T>> {
    let mut out = vec![];
    let mut at = 0;
    for run in keep.chunk_by(|a, b| a == b) {
        let run_items = &items[at..at + run.len()];
        match run[0] {
            true => out.extend(run_items.iter().map(Shown::Item)),
            false => out.push(Shown::Gap(run_items)),
        }
        at += run.len();
    }
    out
}

/// What a gap stands for: `… N more receipt(s)`, and how many failed.
fn gap_label<T>(gap: &[Entry<T>]) -> String {
    let n: usize = gap.iter().map(|e| e.receipts).sum();
    match gap.iter().map(|e| e.failed).sum::<usize>() {
        0 => format!("… {n} more receipt(s)"),
        k => format!("… {n} more receipt(s), {k} failed"),
    }
}

/// The receipt that failed first, as `verdict` names it.
fn first_failure(o: &TxOutcome) -> Option<CryptoHash> {
    o.receipts.iter().find(|r| r.failure_text().is_some()).map(|r| r.receipt_id)
}

impl Tree<'_> {
    /// Refund receipts are left out unless `full` or unhandled; unexecuted
    /// receipts are shown.
    fn shown(&self, id: &CryptoHash) -> bool {
        self.full || self.by_id.get(id).is_none_or(|r| !r.is_refund() || r.unhandled)
    }

    fn render(&mut self, id: CryptoHash, prefix: &str, last: bool) {
        let lead = format!("{prefix}{}", if last { "└─ " } else { "├─ " });
        let Some(r) = self.by_id.get(&id).copied() else {
            let what = match self.pending.get(&id) {
                Some(p) => {
                    let refund = if p.predecessor_id.is_system() { " (refund)" } else { "" };
                    format!(" {}{refund} (not executed yet: {})", p.label(), p.state)
                }
                None => " (not executed yet)".into(),
            };
            let text = format!("{lead}{}{what}\n", short(&id));
            self.units.push(Unit { lead, key: None, text, failed: false, first: false });
            return;
        };
        let mut marks = String::new();
        if let Some(m) = delivered_from(r) {
            marks.push_str(&m);
        } else if r.delivered {
            let n = r.input_data_ids.len();
            let s = if n == 1 { "" } else { "s" };
            marks.push_str(&format!(" [callback of {n} delivered result{s}]"));
        } else if !r.input_data_ids.is_empty() {
            let from: Vec<String> = r
                .input_data_ids
                .iter()
                .map(|d| {
                    self.producer.get(d).map_or("yield/resume".to_string(), |p| receipt_label(p))
                })
                .collect();
            marks.push_str(&format!(" [callback of {}]", from.join(" & ")));
        }
        if let Some(f) = &r.fault {
            marks.push_str(&format!(" [fault: {f}]"));
        }
        let blocks = |n: u64| if n == 1 { "1 block".to_string() } else { format!("{n} blocks") };
        if let Some(n) = r.held {
            marks.push_str(&format!(" [held {}]", blocks(n)));
        }
        if let Some(n) = r.result_held {
            marks.push_str(&format!(" [result held {}]", blocks(n)));
        }
        let failure = r.failure_text();
        let red = self.color && failure.is_some();
        let (status, class) = match (failure, r.value()) {
            (Some(f), _) => {
                // A failure the test forced is excused, as `Sim::findings` excuses it.
                let unhandled = match (r.unhandled, crate::findings::forced(r)) {
                    (true, true) => "  <-- FORCED (excused)",
                    (true, false) => "  <-- UNHANDLED",
                    (false, _) => "",
                };
                let status = format!("FAILED: {f}{unhandled}");
                (status.clone(), status)
            }
            (None, None) => ("ok (returns a promise)".into(), "ok (returns a promise)".into()),
            (None, Some([])) => ("ok".into(), "ok".into()),
            (None, Some(v)) => {
                let max = if self.full { usize::MAX } else { MAX_VALUE_CHARS };
                let v = one_line(&String::from_utf8_lossy(v), max);
                (format!("ok -> {v}"), "ok -> …".into())
            }
        };
        let label = receipt_label(r);
        let line = format!(
            "{lead}#{} {label}{marks}  {}  {}",
            r.block_height,
            gas_summary(r),
            paint(red, RED, &status)
        );
        let mut text = paint(self.color && r.is_refund(), DIM, &line);
        text.push('\n');
        let children: Vec<CryptoHash> =
            r.receipt_ids.iter().copied().filter(|c| self.shown(c)).collect();
        let next = format!("{prefix}{}", if last { "   " } else { "│  " });
        let bar = if children.is_empty() { "   " } else { "│  " };
        // A failed receipt's logs were emitted, but its changes rolled back.
        let log = if r.is_success() { "log" } else { "log (rolled back)" };
        for l in log_lines(&r.logs, self.full) {
            let l = paint(self.color, YELLOW, &format!("{log}: {l}"));
            text.push_str(&format!("{next}{bar}{l}\n"));
        }
        let key = Some(format!("{label}{marks}: {class}"));
        let (failed, first) = (!r.is_success(), Some(r.receipt_id) == self.first);
        self.units.push(Unit { lead, key, text, failed, first });
        for (i, c) in children.iter().enumerate() {
            self.render(*c, &next, i + 1 == children.len());
        }
    }
}

/// How much of a receipt's return value the tree shows, unless `full`.
const MAX_VALUE_CHARS: usize = 40;

/// A receipt's logs for the tree, each on one line. Unless `full`, at most
/// `MAX_LOGS` of them, each cut to `MAX_LOG_CHARS`.
fn log_lines(logs: &[String], full: bool) -> Vec<String> {
    const MAX_LOGS: usize = 10;
    const MAX_LOG_CHARS: usize = 200;
    let keep = if full { logs.len() } else { logs.len().min(MAX_LOGS) };
    let max = if full { usize::MAX } else { MAX_LOG_CHARS };
    let mut lines: Vec<String> = logs[..keep].iter().map(|l| one_line(l, max)).collect();
    if keep < logs.len() {
        lines.push(format!("… {} more log(s)", logs.len() - keep));
    }
    lines
}

/// The variant's name alone, whatever it holds: `Panic`, not the message.
pub(crate) fn variant(v: &impl fmt::Debug) -> String {
    format!("{v:?}").chars().take_while(char::is_ascii_alphanumeric).collect()
}

/// `s` on one line, control characters escaped, cut to `max` characters.
pub(crate) fn one_line(s: &str, max: usize) -> String {
    let n = s.chars().count();
    let mut line = String::new();
    for c in s.chars().take(max) {
        if c.is_control() { line.extend(c.escape_default()) } else { line.push(c) }
    }
    if n > max {
        line.push_str(&format!("… (+{} chars)", n - max));
    }
    line
}

/// A receipt as the trace names it, a refund marked as one: its site
/// (`receiver::method`) if it only calls, its deposit if it only
/// transfers, otherwise each action in order, a call by its method.
pub(crate) fn receipt_label(r: &ReceiptOutcome) -> String {
    let mut methods = r.method_names.iter();
    let names: Vec<&str> = r
        .actions
        .iter()
        .map(|a| match a.as_str() {
            "FunctionCall" => methods.next().expect("a method per call"),
            a => a,
        })
        .collect();
    let calls_only = r.actions.iter().all(|a| a == "FunctionCall");
    let e = &r.receiver_id;
    let label = match (&names[..], calls_only) {
        ([], _) => format!("{e} <- (no actions)"),
        (_, true) => r.site(),
        (["Transfer"], _) => format!("{e} <- transfer {}", near(r.deposit.as_yoctonear())),
        _ => format!("{e} <- {}", names.join(" + ")),
    };
    // A batch is named where it failed, not by its first call.
    let label = match r.failed_method() {
        Some(m) if r.method_names.len() > 1 => format!("{label} (failed at {m})"),
        _ => label,
    };
    if r.is_refund() { label + " (refund)" } else { label }
}

/// `attached A, burnt B (X + F exec fees)`: of `B`, `X` came out of the
/// attached gas and `F` are the exec fees the sender prepaid on top (see
/// [`ReceiptOutcome::prepaid_exec_fees`]). Just `burnt B` without attached
/// gas. A successful call's ends in its [`headroom`](ReceiptOutcome::headroom).
pub(crate) fn gas_summary(r: &ReceiptOutcome) -> String {
    if r.attached_gas == Gas::ZERO {
        return format!("burnt {}", tgas(r.gas_burnt));
    }
    let t = |g: Gas| g.as_gas() as f64 / 1e12;
    let mut s = format!(
        "attached {}, burnt {} ({:.2} + {:.2} exec fees)",
        tgas(r.attached_gas),
        tgas(r.gas_burnt),
        t(r.gas_burnt.saturating_sub(r.prepaid_exec_fees)),
        t(r.prepaid_exec_fees)
    );
    if r.is_success() {
        s.push_str(&format!(", headroom {}", tgas(r.headroom())));
    }
    s
}

/// `SUCCESS`, `FAILURE: ` and the [`Failure`], or `PENDING`.
pub(crate) fn status_label(o: &TxOutcome) -> String {
    match (&o.status, o.failure()) {
        (FinalExecutionStatus::SuccessValue(_), _) => "SUCCESS".into(),
        (_, Some(f)) => format!("FAILURE: {}", failure_line(&f)),
        _ => "PENDING".into(),
    }
}

/// What an outcome that isn't the one asserted did, for the first line of
/// the panic: `first failure: #h receiver::method: why`, naming the
/// receipt (or conversion) that failed first, which may be a hop before
/// the one that failed the transaction; `got success -> value`; or that it
/// is still pending.
pub(crate) fn verdict(o: &TxOutcome) -> String {
    if o.is_pending() {
        return "the transaction is still pending".into();
    }
    if let Some(v) = o.value() {
        return match v {
            [] => "got success".into(),
            v => {
                format!("got success -> {}", one_line(&String::from_utf8_lossy(v), MAX_VALUE_CHARS))
            }
        };
    }
    let conversion = o.transaction.iter().filter_map(|t| {
        let why = t.failure_text()?;
        Some(format!("#{} conversion of a tx signed by {}: {why}", t.block_height, t.receiver_id))
    });
    let receipts = o.receipts.iter().filter_map(|r| {
        let from = delivered_from(r).unwrap_or_default();
        Some(format!("#{} {}{from}: {}", r.block_height, receipt_label(r), r.failure_text()?))
    });
    match conversion.chain(receipts).next() {
        Some(first) => format!("first failure: {first}"),
        None => o.failure().map_or_else(String::new, |f| failure_line(&f)),
    }
}

/// ` [delivered from x]` for a call the simulator delivered as a receipt
/// from `x` (`Tx::predecessor`), which the tree and the headline both mark.
fn delivered_from(r: &ReceiptOutcome) -> Option<String> {
    (r.delivered && r.input_data_ids.is_empty())
        .then(|| format!(" [delivered from {}]", r.predecessor_id))
}

/// A call of `method`, which `receiver`'s contract doesn't export, and the
/// methods it does.
pub(crate) fn missing_method(receiver: &AccountId, method: &str, exports: &[String]) -> String {
    const MAX: usize = 30;
    let mut list = exports.iter().take(MAX).cloned().collect::<Vec<_>>().join(", ");
    if exports.len() > MAX {
        list.push_str(&format!(", … {} more", exports.len() - MAX));
    }
    match exports {
        [] => format!("MethodNotFound: {receiver} has no method {method}; it exports none"),
        _ => format!("MethodNotFound: {receiver} has no method {method}; it exports {list}"),
    }
}

/// `f` on one line, as the trace shows a failure: near-sdk's panic
/// messages span two (`panicked at src/lib.rs:L:C:`, then the message).
pub(crate) fn failure_line(f: &Failure) -> String {
    one_line(&f.to_string(), usize::MAX)
}

pub(crate) fn short(h: &CryptoHash) -> String {
    let s = h.to_string();
    format!("{}…", &s[..6.min(s.len())])
}

pub(crate) fn tgas(g: Gas) -> String {
    format!("{:.2} Tgas", g.as_gas() as f64 / 1e12)
}

pub(crate) fn near(y: u128) -> String {
    if y >= 10u128.pow(20) { format!("{:.4} NEAR", y as f64 / 1e24) } else { format!("{y} yocto") }
}

#[cfg(test)]
mod tests;
