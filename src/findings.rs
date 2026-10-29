//! `Sim::findings`: the built-in checks, read off the block log; and the
//! verdicts and panic capture the receipt scans share.

use crate::inject::Fault;
use crate::outcome::{Failure, ReceiptOutcome, TxOutcome};
use crate::render::{failure_line, gas_summary, near, receipt_label, short, status_label, trace};
use crate::sim::Sim;
use near_primitives::hash::CryptoHash;
use near_primitives::types::AccountId;
use serde::{Deserialize, Serialize};
use std::cell::Cell;
use std::fmt;
use std::ops::Deref;
use std::panic::{self, AssertUnwindSafe};
use std::sync::Once;

/// The names of the built-in checks, as [`Finding::check`] has them.
pub mod builtin {
    /// A receipt failed, and neither a callback nor the caller ever learns
    /// of it.
    pub const UNHANDLED_FAILURE: &str = "unhandled failure";
    /// A callback failed after its contract changed state.
    pub const FAILED_CALLBACK: &str = "failed callback";
    /// NEAR appeared or vanished.
    pub const CONSERVATION: &str = "conservation";
    pub(crate) const ALL: [&str; 3] = [UNHANDLED_FAILURE, FAILED_CALLBACK, CONSERVATION];
}

/// A failed check: which, where, and why.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// The check's name: one of [`builtin`]'s for [`Sim::findings`].
    pub check: String,
    /// Where it failed.
    pub site: Site,
    /// What went wrong, plain text.
    pub message: String,
    /// The block it was found in.
    pub block: u64,
    /// The failed receipt's transaction; `None` for conservation.
    pub tx: Option<CryptoHash>,
    /// The failed receipt; `None` for conservation.
    pub receipt_id: Option<CryptoHash>,
    /// The receipt ran at or after the first of its transaction that
    /// something was forced on: a consequence of the test, not evidence a
    /// network does this.
    pub injected: bool,
    /// How many times it was seen.
    pub occurrences: u32,
    /// The cause its check named ([`Fail::cause`](crate::explore::Fail::cause)).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<String>,
    /// For a check judged at every checkpoint, the block it last passed
    /// at: the receipts after it are the ones that can have broken it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<u64>,
    /// For such a check, the ops submitted when it last passed: those
    /// after them can have broken it. Not saved: an identity is always
    /// taken from a run of this process.
    #[serde(skip)]
    pub(crate) ops_since: Option<usize>,
}

impl Finding {
    /// The key its check gave it ([`Fail::key`](crate::explore::Fail::key)).
    pub fn key(&self) -> Option<&str> {
        match &self.site {
            Site::Key { key, .. } => Some(key),
            _ => None,
        }
    }

    /// A finding of `check` at `site`, at no receipt and nothing forced.
    pub(crate) fn at(
        check: &str,
        message: String,
        site: Site,
        block: u64,
        tx: Option<CryptoHash>,
    ) -> Finding {
        Finding {
            check: check.into(),
            site,
            message,
            block,
            tx,
            receipt_id: None,
            injected: false,
            occurrences: 1,
            cause: None,
            since: None,
            ops_since: None,
        }
    }
}

/// `[check] message`.
impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.check, self.message)
    }
}

/// How a site shows an account the plan made up.
pub(crate) const NEW_ACCOUNT: &str = "<new account>";

/// Where a check failed.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Site {
    /// The whole world: conservation, and a check about no receipt.
    World,
    /// A receipt.
    Receipt {
        /// Its receiver; `None` (`<new account>`) for one the plan made
        /// up, so one behaviour is one finding whatever name was drawn.
        receiver: Option<AccountId>,
        /// Its first called method; `None` for a transfer or a refund.
        method: Option<String>,
        /// The operation whose transaction it belongs to.
        op: Option<String>,
    },
    /// An operation, by name.
    Op(String),
    /// A check's own key ([`Fail::key`](crate::explore::Fail::key)).
    Key {
        /// The key.
        key: String,
        /// The operation, for a per-op check's finding.
        op: Option<String>,
    },
}

/// `world`, `vault.near::on_withdraw`, `alice.near`, `<new account>::m`,
/// each with ` (Op)` when an operation's; the operation; or the key.
impl fmt::Display for Site {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::World => f.write_str("world"),
            Self::Receipt { receiver, method, op } => {
                let receiver = receiver.as_ref().map_or(NEW_ACCOUNT, |r| r.as_str());
                match method {
                    Some(m) => write!(f, "{receiver}::{m}")?,
                    None => write!(f, "{receiver}")?,
                }
                match op {
                    Some(op) => write!(f, " ({op})"),
                    None => Ok(()),
                }
            }
            Self::Op(name) => f.write_str(name),
            Self::Key { key, op } => match op {
                Some(op) => write!(f, "{key} ({op})"),
                None => f.write_str(key),
            },
        }
    }
}

/// Every finding of [`Sim::findings`]; derefs to a slice of [`Finding`].
#[must_use = "findings fail nothing unless asserted: .assert_none()"]
#[derive(Clone, Default)]
pub struct Findings {
    list: Vec<Finding>,
    /// Each finding's transaction, drawn, for `Display`.
    traces: Vec<String>,
}

impl Findings {
    /// Panics, listing each finding with its receipt tree, unless none.
    #[track_caller]
    pub fn assert_none(&self) {
        if !self.list.is_empty() {
            panic!("{} built-in finding(s):\n{self}", self.list.len());
        }
    }
}

impl Deref for Findings {
    type Target = [Finding];
    fn deref(&self) -> &[Finding] {
        &self.list
    }
}

impl IntoIterator for Findings {
    type Item = Finding;
    type IntoIter = std::vec::IntoIter<Finding>;
    fn into_iter(self) -> Self::IntoIter {
        self.list.into_iter()
    }
}

impl<'a> IntoIterator for &'a Findings {
    type Item = &'a Finding;
    type IntoIter = std::slice::Iter<'a, Finding>;
    fn into_iter(self) -> Self::IntoIter {
        self.list.iter()
    }
}

/// Each finding as `[check] message`, then its receipt tree, colored;
/// `no findings` for none.
/// Tests should compare [`Finding::message`], which is plain.
impl fmt::Display for Findings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.list.is_empty() {
            return f.write_str("no findings");
        }
        for (i, (finding, trace)) in self.list.iter().zip(&self.traces).enumerate() {
            if i > 0 {
                f.write_str("\n")?;
            }
            write!(f, "{finding}{trace}")?;
        }
        Ok(())
    }
}

/// As `Display`, so a failed assertion prints each finding in full.
impl fmt::Debug for Findings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// A built-in check's failure as the log gives it, with its outcomes.
#[derive(Clone)]
pub(crate) struct Builtin {
    pub check: &'static str,
    pub message: String,
    pub outcome: Option<TxOutcome>,
    pub receipt: Option<ReceiptOutcome>,
}

/// A fault forced this receipt's failure: it was starved and ran out of
/// gas. One whose inputs a fault changed (a callback receiving
/// `PromiseFailed`) is judged as any.
pub(crate) fn forced(receipt: &ReceiptOutcome) -> bool {
    matches!(receipt.fault, Some(Fault::OutOfGas | Fault::Gas(_)))
        && matches!(receipt.failure(), Some(Failure::OutOfGas | Failure::GasLimitExceeded))
}

/// The block from which something forced on `r` could change what ran.
pub(crate) fn forced_from(r: &ReceiptOutcome) -> Option<u64> {
    let held = r.held.or(r.result_held).map(|n| r.block_height.saturating_sub(n));
    let ran = (r.fault.is_some() || r.delivered).then_some(r.block_height);
    [held, ran].into_iter().flatten().min()
}

impl Sim {
    /// Every finding of the built-in checks over the whole log, in the
    /// order the receipts ran, then conservation.
    ///
    /// - **Unhandled failure** ([`builtin::UNHANDLED_FAILURE`]): a receipt
    ///   failed, and no callback or caller learns of it
    ///   ([`ReceiptOutcome::unhandled`]).
    /// - **Failed callback** ([`builtin::FAILED_CALLBACK`]): a receipt that
    ///   waited on promise results ([`ReceiptOutcome::is_callback`]) failed,
    ///   handled, after an earlier receipt of its transaction changed the
    ///   same contract's state (storage, code, keys, account, a falling
    ///   balance; rewriting a value doesn't count). Its failure undoes only
    ///   its own changes, so the contract keeps what it committed while the
    ///   caller takes the call as failed: two ledgers disagree. A callback
    ///   with nothing to reconcile isn't flagged. A near-sdk contract
    ///   without `#[init]` writes `STATE` in its first `&mut self` call, so
    ///   a callback failing after that call is flagged.
    /// - **Conservation** ([`builtin::CONSERVATION`]): the NEAR in every
    ///   account, gas key and parked receipt is what was minted since
    ///   genesis less what was burnt. The total only, not whose it is:
    ///   ledgers are invariants of your own. Checked only when nothing is
    ///   queued or in flight.
    ///
    /// A receipt a fault starved ([`Fault::OutOfGas`], [`Fault::Gas`],
    /// [`Fault::PromiseFailed`]) that ran out of gas is excused; what it
    /// changed downstream is judged, marked [`injected`](Finding::injected).
    ///
    /// To accept a known failure, filter by the failed receipt:
    ///
    /// ```
    /// # use near_contract_sim::{Balance, Failure, Sim, builtin};
    /// # let mut sim = Sim::new();
    /// # sim.create_account("alice.near", Balance::from_near(10));
    /// # sim.create_account("relayer.near", Balance::from_near(10));
    /// # sim.call("alice.near", "nobody.near", "m").relay("relayer.near");
    /// let known = |f: &&near_contract_sim::Finding| {
    ///     f.check == builtin::UNHANDLED_FAILURE && f.message.contains("does not exist")
    /// };
    /// let unknown: Vec<_> = sim.findings().iter().filter(|f| !known(f)).cloned().collect();
    /// assert!(unknown.is_empty(), "{unknown:?}");
    /// ```
    pub fn findings(&self) -> Findings {
        let mut findings = Findings::default();
        for b in self.builtins_since((0, 0)) {
            if b.receipt.as_ref().is_some_and(forced) {
                continue;
            }
            findings.traces.push(b.outcome.as_ref().map_or(String::new(), trace));
            findings.list.push(self.finding(b));
        }
        findings
    }

    fn finding(&self, b: Builtin) -> Finding {
        let Some(r) = b.receipt else {
            return Finding::at(b.check, b.message, Site::World, self.height(), None);
        };
        let out = b.outcome.unwrap_or_else(|| self.outcome(r.tx));
        // A batch is named by the call it failed at, else its first.
        let method = r.method().map(str::to_owned);
        let first_forced = out.receipts.iter().filter_map(forced_from).min();
        let site = Site::Receipt { receiver: Some(r.receiver_id.clone()), method, op: None };
        Finding {
            receipt_id: Some(r.receipt_id),
            injected: first_forced.is_some_and(|b| r.block_height >= b),
            ..Finding::at(b.check, b.message, site, r.block_height, Some(r.tx))
        }
    }

    /// The built-in checks' failures past the log's `flagged()` counts
    /// `(u, c)`, then conservation if nothing is in flight. Nothing is
    /// excused.
    pub(crate) fn builtins_since(&self, (u, c): (usize, usize)) -> Vec<Builtin> {
        let log = self.chain.log();
        let unhandled = log.unhandled[u..].iter().map(|&at| (at, false));
        let callbacks = log.failed_callbacks[c..].iter().map(|&at| (at, true));
        let mut flagged: Vec<_> = unhandled.chain(callbacks).collect();
        flagged.sort();
        let mut vs: Vec<_> =
            flagged.into_iter().map(|(at, callback)| self.failed_receipt(at, callback)).collect();
        if self.chain.is_settled() {
            vs.extend(self.conservation());
        }
        vs
    }

    fn failed_receipt(&self, at: (usize, usize), callback: bool) -> Builtin {
        let named = |r: &ReceiptOutcome| format!("{} ({})", receipt_label(r), short(&r.receipt_id));
        let r = self.receipt_outcome(at);
        let (check, why) = if callback {
            let log = self.chain.log();
            let wrote: Vec<_> = (log.wrote_before(at).into_iter())
                .map(|p| self.receipt_outcome(p))
                .filter_map(|w| Some(format!("{}: {}", named(&w), log.wrote(&w.receipt_id)?)))
                .collect();
            let why = format!(
                "a callback failed: its contract keeps what it committed before the call it \
                 waited on (in {}), while this failure tells the caller the call failed",
                wrote.join("; ")
            );
            (builtin::FAILED_CALLBACK, why)
        } else {
            let why = "no callback receives this failure and the caller never sees it";
            (builtin::UNHANDLED_FAILURE, why.to_string())
        };
        // An upgrade under an in-flight chain: the code it ran isn't the
        // code its transaction began with.
        let why = match self.chain.log().redeployed(at) {
            Some(h) => format!(
                "{why}; {}'s code changed since its transaction began (deployed at #{h})",
                r.receiver_id
            ),
            None => why,
        };
        let out = self.outcome(r.tx);
        // The finding is of this block: its transaction may finish later.
        let status =
            if out.is_complete() { status_label(&out) } else { "pending at this block".into() };
        let message = format!(
            "{}: {} [{}]\n  transaction status: {status}; {why}",
            named(&r),
            failure_line(&r.failure().expect("a failure")),
            gas_summary(&r),
        );
        Builtin { check, message, outcome: Some(out), receipt: Some(r) }
    }

    fn conservation(&self) -> Option<Builtin> {
        let supply = self.chain.total_supply() as i128;
        let expected = self.chain.log().supply();
        (supply != expected).then(|| Builtin {
            check: builtin::CONSERVATION,
            message: format!(
                "total NEAR supply is {} but minted/burnt accounting expects {} (off by {} yocto)",
                near(supply as u128),
                near(expected.max(0) as u128),
                supply - expected
            ),
            outcome: None,
            receipt: None,
        })
    }
}

thread_local! {
    static GUARDED: Cell<bool> = const { Cell::new(false) };
    static PANICKED_AT: Cell<Option<String>> = const { Cell::new(None) };
}

/// Runs a user closure, its panic its failure:
/// `panicked at {file}:{line}:{column}: {message}`. Shrinking replays it
/// many times, so a hook in front of the process's records the location
/// instead of printing, and passes other panics on.
pub(crate) fn guarded<T>(f: impl FnOnce() -> T) -> Result<T, String> {
    static HOOK: Once = Once::new();
    HOOK.call_once(|| {
        let next = panic::take_hook();
        panic::set_hook(Box::new(move |p| {
            if GUARDED.get() {
                PANICKED_AT.set(p.location().map(ToString::to_string));
            } else {
                next(p);
            }
        }));
    });
    let outer = GUARDED.replace(true);
    let res = panic::catch_unwind(AssertUnwindSafe(f));
    GUARDED.set(outer);
    let at = PANICKED_AT.take().map_or(String::new(), |l| format!(" at {l}"));
    res.map_err(|p| {
        format!("panicked{at}{}", panic_text(&*p).map_or(String::new(), |m| format!(": {m}")))
    })
}

pub(crate) fn panic_text(payload: &(dyn std::any::Any + Send)) -> Option<&str> {
    payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())
}

pub(crate) fn guarding() -> bool {
    GUARDED.get()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn site_display() {
        let receipt = |method: Option<&str>, op: Option<&str>| Site::Receipt {
            receiver: Some("vault.near".parse().unwrap()),
            method: method.map(Into::into),
            op: op.map(Into::into),
        };
        assert_eq!(Site::World.to_string(), "world");
        assert_eq!(receipt(Some("on_withdraw"), None).to_string(), "vault.near::on_withdraw");
        assert_eq!(
            receipt(Some("on_withdraw"), Some("Withdraw")).to_string(),
            "vault.near::on_withdraw (Withdraw)"
        );
        assert_eq!(receipt(None, None).to_string(), "vault.near");
        let new = Site::Receipt { receiver: None, method: Some("m".into()), op: None };
        assert_eq!(new.to_string(), "<new account>::m");
        assert_eq!(Site::Op("Withdraw".into()).to_string(), "Withdraw");
    }

    /// A finding about no receipt is the world's, at the chain's height.
    #[test]
    fn findings_display_each_finding() {
        let f = Finding {
            check: builtin::CONSERVATION.into(),
            site: Site::World,
            message: "m".into(),
            block: 1,
            tx: None,
            receipt_id: None,
            injected: false,
            occurrences: 1,
            cause: None,
            since: None,
            ops_since: None,
        };
        let fs = Findings { list: vec![f.clone(), f], traces: vec![String::new(); 2] };
        assert_eq!(format!("{fs}"), "[conservation] m\n[conservation] m");
        assert_eq!(format!("{fs:?}"), format!("{fs}"));
        assert_eq!(fs.len(), 2);
    }
}
