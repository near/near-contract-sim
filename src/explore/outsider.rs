//! The outsider sweep: a scan sends each call the witnesses made into an
//! audited contract directly from `axis.near` ([`sweep_plans`]), and
//! moves a raw call's deposit and the account ids its JSON args name.
//! [`builtin::SELF_ONLY`] judges what a method only its own contract
//! called does for that outsider. Nothing here builds a call no witness
//! made: a method none reached is named, never guessed at.
//!
//! [`sweep_plans`]: super::axes::sweep_plans
//! [`builtin::SELF_ONLY`]: super::builtin::SELF_ONLY

use super::axes::SIGNER;
use super::plan::{Act, Plan};
use super::{Harness, Op, World};
use crate::Sim;
use crate::outcome::{ReceiptOutcome, TxOutcome};
use near_primitives::hash::CryptoHash;
use near_primitives::types::AccountId;
use serde_json::Value;

/// The id leaves of one call a scan moves, at most: with the six shapes
/// and the five deposits, at most 17 edges of these kinds per raw call.
pub(crate) const ID_LEAVES: usize = 2;

/// A raw call's deposits a scan tries, `sent` being the witness's: 0,
/// 1 yocto, `sent` ∓ 1 and 10 × `sent`, without `sent` itself, repeats,
/// or what over- or underflows.
pub(crate) fn deposits(sent: u128) -> Vec<u128> {
    let mut out: Vec<u128> = [Some(0), Some(1), sent.checked_sub(1), sent.checked_add(1)]
        .into_iter()
        .chain([sent.checked_mul(10)])
        .flatten()
        .filter(|&d| d != sent)
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// The shapes an account id argument is swapped for, by name: the
/// outsider, another account of the fixture (`other`), a 2-char and a
/// 64-char named id, a 64-hex implicit id and an eth-implicit one.
fn shapes(other: Option<&AccountId>) -> Vec<(&'static str, String)> {
    let mut out = vec![("the outsider", SIGNER.to_owned())];
    out.extend(other.map(|o| ("another fixture account", o.to_string())));
    out.extend([
        ("2-char named", "zz".to_owned()),
        ("64-char named", format!("{}.near", "z".repeat(59))),
        ("64-hex implicit", "ab".repeat(32)),
        ("eth implicit", format!("0x{}", "ab".repeat(20))),
    ]);
    out
}

/// Each JSON string of `args` (at any depth) that is a fixture account's
/// id, the first [`ID_LEAVES`] of them with an object's keys in sorted
/// order (serde_json's map), swapped for
/// each shape it isn't already: `(pointer, args, label)`, the pointer
/// under the call's `/args/json`.
pub(crate) fn id_edges(fixture: &Sim, args: &[u8]) -> Vec<(String, Vec<u8>, String)> {
    let Ok(json) = serde_json::from_slice::<Value>(args) else { return vec![] };
    let mut leaves = vec![];
    ids(&json, &mut String::new(), fixture, &mut leaves);
    let users = {
        let mut ids: Vec<&AccountId> = fixture.keys.keys().collect();
        ids.sort();
        ids
    };
    let mut out = vec![];
    for (pointer, id) in leaves.into_iter().take(ID_LEAVES) {
        // Another account, a caller (no code) first.
        let code = |a: &&&AccountId| {
            fixture.account(**a).is_some_and(|v| v.code_hash != Default::default())
        };
        let other = (users.iter().filter(|a| a.as_str() != id && a.as_str() != SIGNER))
            .min_by_key(|a| code(a))
            .copied();
        for (shape, value) in shapes(other) {
            if value == id {
                continue;
            }
            let mut moved = json.clone();
            if let Some(v) = moved.pointer_mut(&pointer) {
                *v = Value::String(value.clone());
            }
            let label = format!("/args/json{pointer} = {value} ({shape})");
            out.push((
                format!("/args/json{pointer}"),
                serde_json::to_vec(&moved).unwrap_or_default(),
                label,
            ));
        }
    }
    out
}

fn ids(v: &Value, path: &mut String, fixture: &Sim, out: &mut Vec<(String, String)>) {
    match v {
        Value::String(s) => {
            let known = s.parse::<AccountId>().ok().filter(|a| fixture.account(a).is_some());
            if known.is_some() {
                out.push((path.clone(), s.clone()));
            }
        }
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                let len = path.len();
                path.push_str(&format!("/{i}"));
                ids(item, path, fixture, out);
                path.truncate(len);
            }
        }
        Value::Object(map) => {
            for (k, item) in map {
                let len = path.len();
                path.push('/');
                path.push_str(&k.replace('~', "~0").replace('/', "~1"));
                ids(item, path, fixture, out);
                path.truncate(len);
            }
        }
        _ => {}
    }
}

/// The site of `plan`'s last step, if it is a raw call from [`SIGNER`]
/// into an audited contract.
pub(crate) fn sent_site<O: Op<W>, W: World>(h: &Harness<O, W>, plan: &Plan<O>) -> Option<String> {
    let Act::Call(c) = &plan.steps.last()?.act else { return None };
    (c.signer.as_str() == SIGNER && h.audited(c.receiver.as_str()))
        .then(|| format!("{}::{}", c.receiver, c.method))
}

/// Whether `r` is the receipt of the call the sweep added to `plan`
/// ([`Plan::swept`]): from [`SIGNER`], at its site.
fn swept_root<O>(plan: &Plan<O>, r: &ReceiptOutcome) -> bool {
    let Some(swept) = &plan.swept else { return false };
    r.predecessor_id.as_str() == SIGNER
        && matches!(&r.method_names[..], [m] if swept.site == format!("{}::{m}", r.receiver_id))
}

/// [`SELF_ONLY`](super::builtin::SELF_ONLY) over a case's `outcomes`: if
/// `plan` sent a self-only method from [`SIGNER`] (but one
/// [`anyone_may_call`](Harness::anyone_may_call) names), and it, or a
/// receipt it led to on the same contract (a call to itself, which
/// passes a predecessor guard), changed that contract's state (storage,
/// code, keys, or a balance it gave away) as the chain's log names it:
/// the receipt, its method and the message. A failed receipt commits
/// nothing, so it never fires.
pub(crate) fn self_only<'a, O: Op<W>, W: World>(
    h: &Harness<O, W>,
    plan: &Plan<O>,
    outcomes: &'a [TxOutcome],
    now: &Sim,
) -> Option<(&'a ReceiptOutcome, String, String)> {
    let swept = plan.swept.as_ref().filter(|s| s.self_only && !h.anyone_may_call_at(&s.site))?;
    let log = now.chain.log();
    let (o, r) = outcomes
        .iter()
        .find_map(|o| Some((o, o.receipts.iter().find(|r| swept_root(plan, r))?)))?;
    let contract = &r.receiver_id;
    // `r` and what it led to on its own contract, in order.
    let mut ids = vec![r.receipt_id];
    for x in &o.receipts {
        if ids.contains(&x.receipt_id) {
            let mine = |c: &&CryptoHash| {
                o.receipts.iter().any(|y| y.receipt_id == **c && y.receiver_id == *contract)
            };
            let next: Vec<CryptoHash> = x.receipt_ids.iter().filter(mine).copied().collect();
            ids.extend(next);
        }
    }
    let named = |id: &CryptoHash| {
        let y = o.receipts.iter().find(|y| y.receipt_id == *id)?;
        let what = log.wrote(id)?;
        Some(if y.receipt_id == r.receipt_id {
            what
        } else {
            format!("{what} (by {})", y.method_names.join(","))
        })
    };
    let changed: Vec<String> = ids.iter().filter_map(named).collect();
    (!changed.is_empty()).then(|| {
        let message = format!(
            "{} succeeded for {SIGNER}, though every witness saw only {contract} call it, and \
             changed {contract}'s state: {}",
            swept.site,
            changed.join("; ")
        );
        (r, r.method_names[0].clone(), message)
    })
}

/// Whether `r` is the call the sweep added to `plan` refusing `axis.near`:
/// failing with nothing forced on it, and not for gas. Not a failure
/// [`must_not_fail`](Harness::must_not_fail) guards against (a guard, or
/// a callback reading a promise result it wasn't given); running out of
/// gas or a limit still is.
pub(crate) fn refused_outsider<O>(plan: &Plan<O>, r: &ReceiptOutcome) -> bool {
    use crate::outcome::Failure;
    let gas = matches!(r.failure(), Some(Failure::OutOfGas | Failure::GasLimitExceeded));
    swept_root(plan, r) && r.fault.is_none() && r.failure().is_some() && !gas
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deposits_drop_the_sent_amount_repeats_and_underflow() {
        assert_eq!(deposits(0), [1]);
        assert_eq!(deposits(1), [0, 2, 10]);
        assert_eq!(deposits(2), [0, 1, 3, 20]);
        assert_eq!(deposits(100), [0, 1, 99, 101, 1000]);
        assert_eq!(deposits(u128::MAX), [0, 1, u128::MAX - 1]);
    }

    /// The first two strings naming an account, keys sorted, at any depth, each swapped for every shape it isn't; another account
    /// is a caller (no code) before a contract.
    #[test]
    fn id_edges_swap_the_first_two_accounts_for_each_shape() {
        let mut sim = Sim::new();
        for id in ["a.near", "alice.near", "bob.near", "zed.near"] {
            sim.create_account(id, crate::Balance::from_near(10));
        }
        sim.deploy("a.near", wat::parse_str("(module)").unwrap());
        let args =
            br#"{"l":[{"to":"alice.near"},"x"],"m":"nobody.near","n":"bob.near","o":"zed.near"}"#;
        let edges = id_edges(&sim, args);
        let labels: Vec<&str> = edges.iter().map(|e| e.2.as_str()).collect();
        let long = format!("{}.near", "z".repeat(59));
        let (hex, eth) = ("ab".repeat(32), format!("0x{}", "ab".repeat(20)));
        let at = |p: &str, v: &str, shape: &str| format!("/args/json{p} = {v} ({shape})");
        let mut want = vec![];
        for (p, other) in [("/l/0/to", "bob.near"), ("/n", "alice.near")] {
            want.extend([
                at(p, "axis.near", "the outsider"),
                at(p, other, "another fixture account"),
                at(p, "zz", "2-char named"),
                at(p, &long, "64-char named"),
                at(p, &hex, "64-hex implicit"),
                at(p, &eth, "eth implicit"),
            ]);
        }
        assert_eq!(labels, want);
        assert_eq!(edges[0].0, "/args/json/l/0/to");
        let moved: Value = serde_json::from_slice(&edges[0].1).unwrap();
        assert_eq!(moved["l"][0]["to"], "axis.near");
        assert_eq!(moved["n"], "bob.near");
        assert_eq!(long.len(), 64);
        // An id already a shape isn't swapped for itself; bytes aren't JSON.
        sim.create_account("axis.near", crate::Balance::from_near(10));
        let own = id_edges(&sim, br#"{"to":"axis.near"}"#);
        assert_eq!(own.len(), 5, "{own:?}");
        assert!(id_edges(&sim, b"alice.near").is_empty());
    }
}
