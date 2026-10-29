//! Margin on callbacks that must not fail. A callback's gas budget is
//! fixed when it is scheduled; what it uses can depend on more than the
//! budget's formula sees (its arguments' bytes, entries others appended
//! while it waited). Its margin, what it keeps of its attached gas, falls
//! along an unpaid axis; a combined ascent moves all of a target's falling
//! axes together. A margin under [`MARGIN`] is a finding though nothing
//! crossed: the sim's pricing isn't a network's.

use super::{Audit, AxisKind, CurveRow, Curves, How, Reach, Resource, Run, StepGas};
use super::{Op, World, moved};
use crate::explore::{Inject, run::address};
use crate::inject::Fault;
use crate::outcome::{Failure, ReceiptOutcome};
use near_primitives::types::AccountId;
use serde::{Deserialize, Serialize};

/// A target whose worst margin is under this percent of its attached gas
/// is a finding: the sim prices storage by trie nodes, which some
/// sandboxes and networks price differently (fidelity.md).
pub(crate) const MARGIN: u32 = 25;
/// Runs a target's combined ascent takes, at most, per witness.
const ASCENT: u32 = 16;

/// A margin a plan's run judges: a receipt at `site` that keeps under
/// `under` percent of its attached gas is a finding of
/// [`builtin::MARGIN`](crate::explore::builtin::MARGIN); `why` is what
/// found it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Margin {
    pub site: String,
    pub under: u32,
    pub why: String,
}

/// What `r`, if it runs at `site`, keeps of its attached gas ([`left`];
/// -1 where it ran out), and that gas.
pub(crate) fn kept(r: &ReceiptOutcome, site: &str) -> Option<(i128, i128)> {
    r.method_names.iter().any(|m| format!("{}::{m}", r.receiver_id) == site).then_some(())?;
    let (attached, fw) = (r.attached_gas.as_gas().into(), r.forwarded.as_gas().into());
    let own = r.gas_burnt.saturating_sub(r.prepaid_exec_fees).as_gas().into();
    let out = matches!(r.failure(), Some(Failure::OutOfGas | Failure::GasLimitExceeded));
    Some((if out { -1 } else { left(attached, own, fw) }, attached as i128))
}

/// Attached less used less forwarded; a receipt that forwarded exactly
/// what it didn't use (by gas weight) keeps what it didn't use.
pub(crate) fn left(attached: u128, used: u128, forwarded: u128) -> i128 {
    let (a, u, f) = (attached as i128, used as i128, forwarded as i128);
    if a - u - f == 0 && f > 0 { a - u } else { a - u - f }
}

/// `left` of `attached`, in percent.
fn pct(left: i128, attached: i128) -> i128 {
    left.max(0) * 100 / attached.max(1)
}

/// `left` of `attached`, in millionths: what an ascent lowers.
fn ppm(left: i128, attached: i128) -> i128 {
    left * 1_000_000 / attached.max(1)
}

/// Whether `a` keeps a smaller share of its attached gas than `b`.
fn lower<O>(a: &Best<O>, b: &Best<O>) -> bool {
    ppm(a.0, a.1) < ppm(b.0, b.1)
}

/// The receipt of `receipts` at `m.site` that keeps the least, with the
/// finding's message, if it keeps under `m.under` percent.
pub(crate) fn judge<'a>(
    m: &Margin,
    receipts: impl Iterator<Item = &'a ReceiptOutcome>,
) -> Option<(&'a ReceiptOutcome, String)> {
    let worst =
        receipts.filter_map(|r| kept(r, &m.site).map(|k| (k, r))).min_by_key(|x| ppm(x.0.0, x.0.1));
    let ((left, attached), r) = worst.filter(|((l, a), _)| l * 100 < i128::from(m.under) * a)?;
    let kept = if left < 0 { "runs out".into() } else { format!("keeps {}%", pct(left, attached)) };
    let (of, why) = (crate::render::tgas(r.attached_gas), m.why.replace('\n', "\n  "));
    // Over the threshold, the finding is an axis whose cost outgrows its budget.
    let under = format!("under {}%", m.under);
    let under =
        if m.under > MARGIN { "its cost growing faster than its budget".into() } else { under };
    let by = "found by margin, not by crossing, under the sim's gas pricing (fidelity.md)";
    Some((r, format!("{} {kept} of its {of}, {under}: {by}\n  {why}", m.site)))
}

/// One axis of a target's rows: its step, the axis, its value where the
/// margin is least, a line of its slopes, and whether it is unpaid.
type Leg = (usize, super::Axis, u128, String, bool);

/// `row`'s leg, unpaid if its margin falls by more than 1% of the
/// attached gas and keeps falling: by over 0.1% between two pairs of
/// neighbouring points at least (one step isn't growth); `None` for the
/// caller's gas, a choice, or no effect.
fn leg<O>(row: &CurveRow<O>) -> Option<Leg> {
    let skip = matches!(row.axis.kind, AxisKind::CallerGas | AxisKind::Choice);
    let margins = row.margins().into_iter().zip(&row.limits);
    let ((v0, m0), b0) =
        margins.clone().next().filter(|_| !skip && row.reach != Reach::NoEffect)?;
    let ((vw, mw), bw) = margins.min_by_key(|((_, m), b)| ppm(*m, **b as i128))?;
    let per = |a: i128, b: i128| (b - a) as f64 / (vw.saturating_sub(v0).max(1)) as f64;
    let (cost, budget) = (per(*b0 as i128 - m0, *bw as i128 - mw), per(*b0 as i128, *bw as i128));
    // Where the cost, growing faster than the budget, would meet it.
    let how = ["grows faster, meets the budget", "grows, budget flat, meets it"]
        [usize::from(budget == 0.0)];
    let near = vw as f64 + mw as f64 / (cost - budget);
    let meets = if cost > budget { format!(": cost {how} near {near:.0}") } else { String::new() };
    let line = format!(
        "{} at step {}: {}% at {vw}; cost {cost:+.3e} gas a unit, budget {budget:+.3e}{meets}",
        row.axis,
        row.step,
        pct(mw, *bw as i128),
    );
    let falls = row.margins().windows(2).filter(|w| w[0].1 - w[1].1 > *b0 as i128 / 1000).count();
    Some((row.step, row.axis.clone(), vw, line, m0 - mw > *b0 as i128 / 100 && falls > 1))
}

impl<O: Op<W>, W: World> Curves<'_, O, W> {
    /// For each witness, each margin target's legs, their combined ascent,
    /// and its finding: an unpaid axis, or a margin under [`MARGIN`].
    /// `of` is each row's witness.
    /// A target of one witness is every witness's (a direct witness
    /// reaches it with its own lists and tails).
    pub(super) fn margins(&self, witnesses: &[&Run<O>], of: &[usize], audit: &mut Audit<O>) {
        let phase = crate::explore::progress::phase("the gas margins".into(), 0, None);
        let targets: std::collections::BTreeSet<String> =
            witnesses.iter().flat_map(|w| self.targets(w, audit)).collect();
        let total = (witnesses.len() * targets.len()) as u32;
        for (w, witness) in witnesses.iter().enumerate() {
            for (t, site) in targets.iter().enumerate() {
                phase.step((w * targets.len() + t) as u32, total);
                let rows: Vec<_> = (audit.curves.iter().zip(of))
                    .filter(|(r, k)| {
                        **k == w && r.site == *site && r.resource == Resource::ReceiptGas
                    })
                    .collect();
                let legs: Vec<Leg> = rows.iter().filter_map(|(r, _)| leg(r)).collect();
                if !rows.is_empty() {
                    self.ascend(witness, site, &legs, audit);
                }
            }
        }
    }

    /// The callbacks of `witness` where a forced `OutOfGas` shows a
    /// finding other than its own failure, and those that must not fail.
    fn targets(&self, witness: &Run<O>, audit: &Audit<O>) -> Vec<String> {
        let mut out: Vec<String> = (audit.curves.iter().map(|r| r.site.clone()))
            .filter(|s| self.harness.must_not_fail_at(s))
            .collect();
        let other = crate::findings::builtin::UNHANDLED_FAILURE;
        for (i, txs) in witness.spawned.iter().enumerate() {
            for (tx, (spawned, _)) in txs.iter().enumerate() {
                for (n, s) in spawned.iter().enumerate().filter(|(_, s)| s.awaits > 0) {
                    let (site, fault, mut p) = (s.site(), Fault::OutOfGas, witness.plan.clone());
                    let at = address(spawned, n, &fault);
                    p.steps[i].inject.push(Inject { tx: tx as u8, at, fault });
                    if s.methods.len() == 1
                        && !out.contains(&site)
                        && self.harness.run_case(&p).findings.iter().any(|f| f.check != other)
                    {
                        out.push(site);
                    }
                }
            }
        }
        out.sort();
        out.dedup();
        out
    }

    /// `witness`, at the ceiling's gas, with each leg at `values` and its
    /// least margin at `site`: arguments first, then repeats and tails
    /// from the last step back, so each leg's step is where the witness
    /// had it.
    fn try_at(
        &self,
        witness: &Run<O>,
        site: &str,
        legs: &[Leg],
        values: &[u128],
    ) -> Option<Best<O>> {
        let mut p = witness.plan.clone();
        for (step, _) in p.steps.iter_mut().zip(&witness.ops).filter(|(_, o)| o.unpinned > 0) {
            step.gas = Some(StepGas::Exactly(self.harness.gas_ceiling()));
        }
        let mut order: Vec<usize> = (0..legs.len()).collect();
        let copies = |k: &usize| matches!(legs[*k].1.how, How::Repeat(_) | How::Tail { .. });
        order.sort_by_key(|k| (copies(k), std::cmp::Reverse(legs[*k].0)));
        for k in order {
            p = moved::<O, W>(&p, legs[k].0, &legs[k].1, values[k])?;
        }
        let run = self.harness.run_case(&p);
        let receipts = run.outcomes.iter().flat_map(|o| &o.receipts);
        let (left, attached) =
            receipts.filter_map(|r| kept(r, site)).min_by_key(|x| ppm(x.0, x.1))?;
        Some((left, attached, values.to_vec(), run))
    }

    /// From the witness, each leg moved to where its margin is least, kept
    /// if the margin falls, then doubled while it does, within [`ASCENT`]
    /// runs. Prints the worst margin; its finding, if any, goes to `audit`.
    fn ascend(&self, witness: &Run<O>, site: &str, legs: &[Leg], audit: &mut Audit<O>) {
        let start: Vec<u128> = legs.iter().map(|l| l.1.at).collect();
        let Some(mut best) = self.try_at(witness, site, legs, &start) else { return };
        let mut tries: Vec<(usize, u128)> =
            legs.iter().enumerate().filter(|(_, l)| l.2 != l.1.at).map(|(k, l)| (k, l.2)).collect();
        let mut runs = 1;
        while let Some((k, v)) = (runs < ASCENT).then(|| tries.pop()).flatten() {
            runs += 1;
            let mut next = best.2.clone();
            next[k] = v;
            if let Some(b) = self.try_at(witness, site, legs, &next).filter(|b| lower(b, &best)) {
                best = b;
                let top = match legs[k].1.kind {
                    AxisKind::Repeat | AxisKind::HoldTail => super::COPIES,
                    _ => 1 << 16,
                };
                tries.insert(0, (k, v.saturating_mul(2).min(top).max(v + 1)));
            }
        }
        // The shaper: the trie forked along each key the target wrote, in
        // an audited contract only: keys planted in a counterparty's state
        // could break its own reads.
        let clean = pct(best.0, best.1);
        let receipts = best.3.outcomes.iter().flat_map(|o| &o.receipts);
        let audited = |a: &AccountId| {
            self.harness.audits.as_ref().is_none_or(|x| x.iter().any(|x| x == a.as_str()))
        };
        let written = (receipts.filter(|r| kept(r, site).is_some()))
            .flat_map(|r| r.storage_ops().written)
            .filter(|w| audited(&w.account));
        let mut shaped = best.3.plan.clone();
        shaped.planted = written.flat_map(|w| trie_siblings(&w.account, &w.key)).collect();
        let n = shaped.planted.len();
        let run = (n > 0).then(|| self.harness.run_case(&shaped)).filter(|r| r.panicked.is_none());
        let low = run.and_then(|run| {
            let receipts = run.outcomes.iter().flat_map(|o| &o.receipts);
            let (l, a) = receipts.filter_map(|r| kept(r, site)).min_by_key(|x| ppm(x.0, x.1))?;
            Some((l, a, best.2.clone(), run))
        });
        if let Some(b) = low.filter(|b| lower(b, &best)) {
            best = b;
        }
        let (left, attached, values, run) = best;
        let shaped = if run.plan.planted.is_empty() { "" } else { ", trie shaped" };
        let moved = legs.iter().zip(&values).filter(|(l, v)| **v != l.1.at);
        let moved: Vec<String> = moved.map(|(l, v)| format!("{} = {v}", l.1.name)).collect();
        let unpaid: Vec<&str> = legs.iter().filter(|l| l.4).map(|l| l.1.name.as_str()).collect();
        let (unpaid, moved) = (unpaid.join(", "), moved.join(", ") + shaped);
        let p = pct(left, attached);
        eprintln!(
            "margin of {site}: worst {p}% ({left} gas) at [{moved}]; clean trie {clean}%, \
             {n} siblings planted; unpaid: [{unpaid}]"
        );
        if p >= i128::from(MARGIN) && unpaid.is_empty() {
            return;
        }
        let under = if p < i128::from(MARGIN) { MARGIN } else { p as u32 + 1 };
        let lines: Vec<&str> = legs.iter().map(|l| l.3.as_str()).collect();
        let why = format!(
            "unpaid along [{unpaid}]; combined [{moved}]; {clean}% with a clean trie, {p}% with \
             {n} siblings forking it along the keys it writes\n{}",
            lines.join("\n")
        );
        let mut plan = run.plan.clone();
        plan.margin = vec![Margin { site: site.to_owned(), under, why }];
        let at = format!("margin of {site}");
        audit.add(&self.harness.run_case(&plan), Some((&at, &at)));
    }
}

/// Contract-data keys that fork the trie along `account`'s `key`, the
/// margin's trie shaper: plant them (`Sim::apply_state_records`) and a
/// write or remove of the key pays trie nodes (2.28 Ggas each) at every
/// nibble of its path, 2 to 4 per byte; a read pays none (flat storage). Per byte
/// of the account id and the `,` after it, an id diverging there (`q.near`
/// after it); per byte of the key, up to 2048 (the protocol's longest key), its prefix diverging
/// there. Each byte forks at either nibble.
pub fn trie_siblings(account: &AccountId, key: &[u8]) -> Vec<(AccountId, Vec<u8>)> {
    let path = format!("{account},");
    let mut out: Vec<(AccountId, Vec<u8>)> = vec![];
    for (j, b) in path.bytes().enumerate() {
        let high = if b.is_ascii_lowercase() { b'0' } else { b'a' };
        for c in [high, b ^ 1, b ^ 3] {
            let id = format!("{}{}q.near", &path[..j], c as char);
            out.extend(id.parse().ok().filter(|i| i != account).map(|i| (i, key.to_vec())));
        }
    }
    for j in 0..key.len().min(KEY_MAX) {
        for x in [0x80, 0x01] {
            let mut k = key[..=j].to_vec();
            k[j] ^= x;
            out.push((account.clone(), k));
        }
    }
    out.sort();
    out.dedup();
    out
}

/// The protocol's longest storage key (`max_length_storage_key`): the
/// shaper forks a key along all of its bytes, up to this many.
const KEY_MAX: usize = 2048;

/// A combined point: its margin and attached gas, each leg's value, and
/// the run.
type Best<O> = (i128, i128, Vec<u128>, Run<O>);

#[cfg(test)]
mod tests {
    use super::left;

    /// A receipt that forwarded exactly what it didn't use (gas weight)
    /// keeps that; one with a static forward keeps what is left after it.
    #[test]
    fn a_weight_forward_keeps_what_it_did_not_use() {
        assert_eq!(left(100, 30, 70), 70);
        assert_eq!(left(100, 30, 50), 20);
        assert_eq!(left(100, 30, 0), 70);
        assert_eq!(left(100, 60, 50), -10);
    }
}
