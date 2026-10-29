//! Holds (`Fault::Hold`, `Fault::HoldUntil`, `Fault::HoldResult`): receipts
//! kept back between blocks, as nearcore's outgoing buffer keeps one back.
//! `fault.rs` picks them as they leave the block that creates them; here
//! they wait, then go out in the order they were held, ahead of the block's
//! new receipts, as nearcore forwards its buffer before the receipts a
//! chunk creates. The log notes how many blocks each was kept back, which
//! its outcome shows.

use super::Chain;
use near_primitives::hash::CryptoHash;
use near_primitives::receipt::{Receipt, ReceiptEnum};
use std::collections::HashMap;

/// A `HoldUntil` released though it couldn't apply: transaction `tx`'s
/// injection `i`, at receipt `id` (`site`), and why.
pub(super) struct Refused {
    tx: CryptoHash,
    i: usize,
    id: CryptoHash,
    site: String,
    why: String,
}

/// What a held receipt waits for.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Wait {
    /// This many more blocks.
    Blocks(u64),
    /// The receipt transaction `tx`'s injection `i` (a `HoldUntil`) names,
    /// in `tx` or the transaction its `in_tx` names, to have run.
    Until { tx: CryptoHash, i: usize },
}

impl Chain {
    /// Sets the next block's incoming receipts, from this block's
    /// `outgoing`: first the held receipts now due, in the order they were
    /// held, then `outgoing` less the receipts `hold` keeps back. A held
    /// receipt is due once its blocks have passed, or the receipt it waits
    /// for has run (`ran` is this block's), and every receipt held before
    /// it has gone, as a buffer drains from its head. When nothing else is
    /// in flight, a head waiting for a receipt that hasn't run goes anyway:
    /// its hold can't apply, which it returns, for `refuse_hold` once the
    /// block is logged. Those behind it go from the next block on. Called before the block is pushed to the log, and
    /// after the congestion info is carried.
    pub(super) fn hold_back(
        &mut self,
        outgoing: Vec<Receipt>,
        hold: &HashMap<CryptoHash, Wait>,
        ran: &[CryptoHash],
    ) -> Option<Refused> {
        let now = self.log.blocks.len();
        for (_, wait, _) in &mut self.holding {
            if let Wait::Blocks(blocks) = wait {
                *blocks = blocks.saturating_sub(1);
            }
        }
        let mut incoming = vec![];
        while self.holding.front().is_some_and(|(_, w, _)| self.due(w, ran)) {
            let (r, _, at) = self.holding.pop_front().expect("a head");
            incoming.push(self.release(r, at, now));
        }
        for r in outgoing {
            match hold.get(r.receipt_id()) {
                Some(&wait) => self.holding.push_back((r, wait, now)),
                None => incoming.push(r),
            }
        }
        self.incoming = incoming;
        if !self.is_quiet() {
            return None;
        }
        let &(_, Wait::Until { tx, i }, at) = self.holding.front()? else { return None };
        let (r, _, _) = self.holding.pop_front().expect("a head");
        let why = match self.target(&tx, i) {
            Some(t) => format!("receipt {t}, which it waits for, never ran"),
            None => "nothing it waits for left a block".into(),
        };
        let why = match self.holding.len() {
            0 => format!("{why}, and nothing else is in flight"),
            n => format!(
                "{why}, and nothing else is in flight but the {n} held behind it, which can't \
                 leave before it (holds go out first in, first out)"
            ),
        };
        let methods = super::action_receipt(&r).map(|a| super::method_names(a.actions()));
        let site = crate::outcome::site(r.receiver_id(), &methods.unwrap_or_default());
        let id = *r.receipt_id();
        let r = self.release(r, at, now);
        self.incoming.push(r);
        Some(Refused { tx, i, id, site, why })
    }

    /// Records a hold `hold_back` released though it couldn't apply, or
    /// panics if its transaction isn't lenient: after the block is logged,
    /// so the chain stays whole.
    pub(super) fn refuse_hold(&mut self, Refused { tx, i, id, site, why }: Refused) {
        self.faults.get_mut(&tx).expect("its faults").refuse(i, id, &site, &why);
    }

    /// Whether a receipt held for `wait` may go out after this block.
    fn due(&self, wait: &Wait, ran: &[CryptoHash]) -> bool {
        match *wait {
            Wait::Blocks(blocks) => blocks == 0,
            Wait::Until { tx, i } => self
                .target(&tx, i)
                .is_some_and(|t| ran.contains(&t) || self.log.position(&t).is_some()),
        }
    }

    /// `r`, held in block `at`, to send into the next block, `now + 1`,
    /// noting how late.
    fn release(&mut self, r: Receipt, at: usize, now: usize) -> Receipt {
        let late = (now - at) as u64;
        match r.receipt() {
            ReceiptEnum::Data(d) => self.log.held_results.insert(d.data_id, late),
            _ => self.log.held.insert(*r.receipt_id(), late),
        };
        r
    }

    pub(crate) fn is_holding(&self) -> bool {
        !self.holding.is_empty()
    }
}
