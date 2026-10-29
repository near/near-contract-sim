//! Injections (`Tx::inject`), applied where a receipt a transaction spawned
//! leaves the block that created it: every such receipt runs a block later
//! at the earliest, so its gas, the calls its callback waits on and the
//! data receipts that carry their results can be rewritten before nearcore
//! runs them. Gas a fault takes from a receipt goes into its creator's gas
//! refund, sent in the same block, so the supply ledger needs nothing.
//! The calls are first-version `Action` receipts (a delegate's inner one)
//! or `ActionV2` ones (what a contract creates), and a fault treats both
//! alike.
//!
//! Each receipt has three slots, what it runs with (`PromiseFailed`,
//! `OutOfGas`, `Gas`, `ReturnValue`), when it leaves (`Hold`, `HoldUntil`)
//! and when its result leaves (`HoldResult`), and gets the first injection
//! that selects it for each. Holds are picked here and kept back by
//! `order.rs`.

use super::order::Wait;
use super::{Chain, action_receipt, calls, is_gas_refund};
use crate::fault::Spawned;
use crate::inject::{Fault, Receipts};
use near_primitives::action::{Action, TransferAction};
use near_primitives::hash::{CryptoHash, hash};
use near_primitives::receipt::{Receipt, ReceiptEnum};
use near_primitives::transaction::ExecutionOutcomeWithId;
use near_primitives::types::{Balance, Gas};
use std::collections::{HashMap, HashSet};

/// A transaction's injections and what they have met so far.
#[derive(Clone, Default)]
pub(crate) struct Faulting {
    pub injections: Vec<(Receipts, Fault)>,
    /// Per injection with an `nth`, how many receipts it selected and its
    /// fault fit so far.
    seen: Vec<usize>,
    /// Every receipt it spawned, in the order they left their blocks:
    /// what `Receipts::index` counts.
    pub spawned: Vec<Spawned>,
    /// A fault that can't apply is passed over instead of panicking, as an
    /// explored plan step's injection requires.
    pub lenient: bool,
    /// Each fault passed over so, and why: `<receipt>: <why>`.
    pub passed: Vec<(Fault, String)>,
    /// Each injection that applied, by position, with the receipt it
    /// applied to: for a `HoldResult`, the receipt whose result it held.
    pub won: Vec<(usize, CryptoHash)>,
    /// Its receipts congestion control held back in the outgoing buffer,
    /// which no fault or hold reaches.
    pub buffered: Vec<Spawned>,
    /// Data ids whose successful result `ReturnValue` replaces.
    returns: HashMap<CryptoHash, Vec<u8>>,
    /// The spawned receipts, by index, whose result a `HoldResult` holds,
    /// and for how many blocks.
    result_holds: HashMap<usize, u64>,
    /// Each data id a spawned receipt sends its result under, with the
    /// receipts, by index, that send it: the call, and any it handed its
    /// result on to by returning a promise.
    results: HashMap<CryptoHash, Vec<usize>>,
}

impl Faulting {
    pub(crate) fn new(injections: Vec<(Receipts, Fault)>) -> Self {
        let seen = vec![0; injections.len()];
        Faulting { injections, seen, ..Default::default() }
    }

    /// For the `n`th spawned receipt `s`, the injection that wins each of
    /// its slots, by position: the first whose selector selects it. An
    /// `nth` selector counts, and so selects, only receipts its fault fits,
    /// whichever injection wins them.
    fn select(&mut self, n: usize, s: &Spawned) -> [Option<usize>; 3] {
        let mut won = [None; 3];
        for (i, (at, fault)) in self.injections.iter().enumerate() {
            if !at.matches(n, s) {
                continue;
            }
            if let Some(k) = at.nth_of() {
                if !s.fits(fault) {
                    continue;
                }
                self.seen[i] += 1;
                if self.seen[i] != k + 1 {
                    continue;
                }
            }
            won[fault.slot()].get_or_insert(i);
        }
        won
    }

    /// Records that injection `i`'s hold of `id` can't apply, and why;
    /// panics unless lenient.
    pub(crate) fn refuse(&mut self, i: usize, id: CryptoHash, site: &str, why: &str) {
        let (at, fault) = &self.injections[i];
        assert!(self.lenient, "{fault} at {at} ({site}): {why}");
        self.won.retain(|w| *w != (i, id));
        self.passed.push((fault.clone(), format!("{site}: {why}")));
    }
}

/// A fault that won receipt `j`'s run slot (injection `i` of transaction
/// `tx`), applied once every receipt of the block is selected.
struct Due {
    tx: CryptoHash,
    i: usize,
    j: usize,
    at: Receipts,
    fault: Fault,
    site: String,
    /// The promise results the receipt waits on.
    inputs: Vec<CryptoHash>,
    lenient: bool,
}

impl Spawned {
    /// `r` as it leaves a block whose receipts are `outgoing`, `producer`
    /// naming the one of them whose result each data id carries.
    fn leaving(r: &Receipt, outgoing: &[Receipt], producer: &HashMap<CryptoHash, usize>) -> Self {
        let fcs = function_calls(r);
        let inputs = input_data_ids(r).unwrap_or_default();
        let calls = |d: &CryptoHash| producer.get(d).map(|&k| function_calls(&outgoing[k]).len());
        Spawned {
            id: *r.receipt_id(),
            receiver: r.receiver_id().clone(),
            methods: fcs.iter().map(|c| c.0.clone()).collect(),
            actions: action_kinds(r),
            attached: fcs.iter().fold(Gas::ZERO, |g, c| g.saturating_add(c.1)),
            awaits: inputs.len(),
            failable: inputs.iter().all(|d| calls(d).is_some_and(|n| n > 0)),
            awaits_no_call: no_call(&inputs, outgoing, producer),
            sends: !output_data_ids(r).is_empty(),
            awaits_from: inputs
                .iter()
                .filter_map(|d| producer.get(d).map(|&k| outgoing[k].receiver_id().clone()))
                .collect(),
        }
    }
}

/// Whether a receipt waiting on `inputs` waits on one that left in this
/// block and makes no function call (a transfer, say): its result is
/// always empty. One that left an earlier block isn't seen.
fn no_call(
    inputs: &[CryptoHash],
    outgoing: &[Receipt],
    producer: &HashMap<CryptoHash, usize>,
) -> bool {
    inputs.iter().any(|d| producer.get(d).is_some_and(|&k| function_calls(&outgoing[k]).is_empty()))
}

/// nearcore sends no empty gas refund: drops one a raise emptied, from
/// the block's receipts and its creator's outcome.
fn drop_empty_refunds(executed: &mut [ExecutionOutcomeWithId], outgoing: &mut Vec<Receipt>) {
    let empty: HashSet<CryptoHash> = outgoing
        .iter()
        .filter(|r| is_gas_refund(r) && refund(r) == Balance::ZERO)
        .map(|r| *r.receipt_id())
        .collect();
    if !empty.is_empty() {
        outgoing.retain(|r| !empty.contains(r.receipt_id()));
        for o in executed.iter_mut() {
            o.outcome.receipt_ids.retain(|c| !empty.contains(c));
        }
    }
}

impl Chain {
    /// The receipts transaction `tx` spawned so far: none for one submitted
    /// before the sim was loaded from a snapshot.
    pub(crate) fn spawned(&self, tx: &CryptoHash) -> &[Spawned] {
        match self.faults.get(tx) {
            Some(f) => &f.spawned,
            None => self.history.get(tx).map_or(&[], Vec::as_slice),
        }
    }

    /// Whether `tx`'s spawned receipts are recorded: not for one submitted
    /// before the sim was loaded from a snapshot (which is taken settled).
    fn recorded(&self, tx: &CryptoHash) -> bool {
        self.faults.contains_key(tx) || self.history.contains_key(tx)
    }

    /// The receipt transaction `tx`'s injection `i`, a `HoldUntil`, waits
    /// for, once it has left its block: the first (or `nth`) receipt its
    /// selector selects among those `tx` spawned, or the transaction its
    /// `in_tx` names, counting every one it selects.
    pub(crate) fn target(&self, tx: &CryptoHash, i: usize) -> Option<CryptoHash> {
        let Fault::HoldUntil(at) = &self.faults[tx].injections[i].1 else { return None };
        let spawned = self.spawned(&at.tx_of().unwrap_or(*tx));
        let mut selected = spawned.iter().enumerate().filter(|(n, s)| at.matches(*n, s));
        selected.nth(at.nth_of().unwrap_or(0)).map(|(_, s)| s.id)
    }

    /// Transaction `tx`'s receipts congestion control held back.
    pub(crate) fn buffered(&self, tx: &CryptoHash) -> &[Spawned] {
        self.faults.get(tx).map_or(&[], |f| &f.buffered)
    }

    /// The faults `tx`'s lenient injections passed over, each with the
    /// receipt and why it couldn't apply there.
    pub(crate) fn passed(&self, tx: &CryptoHash) -> &[(Fault, String)] {
        self.faults.get(tx).map_or(&[], |f| &f.passed)
    }

    /// Each of `tx`'s injections that applied, by position, with the
    /// receipt it applied to (for a `HoldResult`, the one whose result it
    /// held): one fault never counts for two injections.
    pub(crate) fn won(&self, tx: &CryptoHash) -> &[(usize, CryptoHash)] {
        self.faults.get(tx).map_or(&[], |f| &f.won)
    }

    /// Makes `tx`'s injections pass over a fault that can't apply. Called
    /// as it is submitted, before it spawns anything, so its `history`
    /// entry is empty: `faults` records what it spawns from here on.
    pub(crate) fn lenient(&mut self, tx: &CryptoHash) {
        self.history.remove(tx);
        self.faults.entry(*tx).or_default().lenient = true;
    }

    /// Records each receipt this block sends with the transaction that
    /// spawned it, and applies the injections of the transactions that
    /// have some: `executed` is the block's outcomes,
    /// conversions included, and `buffered` the receipts congestion control
    /// kept back. Returns the receipts to hold (`Fault::Hold`,
    /// `Fault::HoldUntil`, `Fault::HoldResult`), by id, each with what it
    /// waits for.
    pub(super) fn apply_faults(
        &mut self,
        executed: &mut [ExecutionOutcomeWithId],
        outgoing: &mut Vec<Receipt>,
        buffered: &[Receipt],
    ) -> HashMap<CryptoHash, Wait> {
        let mut hold = HashMap::new();
        if self.faults.is_empty() && self.history.is_empty() {
            return hold;
        }
        // Each receipt created in this block: its transaction, and the
        // position of its creator's outcome. A block's outcomes come parent
        // first. A transaction's own receipt is no fault's.
        let (mut root, mut creator, mut own) =
            (HashMap::new(), HashMap::new(), HashSet::<CryptoHash>::new());
        for (i, o) in executed.iter().enumerate() {
            let tx = if self.log.txs.contains_key(&o.id) {
                own.extend(o.outcome.receipt_ids.first());
                o.id
            } else {
                match root.get(&o.id).or(self.log.origin.get(&o.id)) {
                    Some(tx) => *tx,
                    None => continue,
                }
            };
            for c in &o.outcome.receipt_ids {
                root.insert(*c, tx);
                creator.insert(*c, i);
            }
        }
        // The receipt whose result each data id carries.
        let mut producer = HashMap::new();
        for (j, r) in outgoing.iter().enumerate() {
            for d in output_data_ids(r) {
                producer.insert(d, j);
            }
        }
        // What each transaction without faults spawned, for a later one's
        // `HoldUntil` (`Receipts::in_tx`).
        for r in outgoing.iter() {
            let Some(h) = root.get(r.receipt_id()).and_then(|t| self.history.get_mut(t)) else {
                continue;
            };
            if !own.contains(r.receipt_id())
                && !r.predecessor_id().is_system()
                && input_data_ids(r).is_some()
            {
                h.push(Spawned::leaving(r, outgoing, &producer));
            }
        }
        if self.faults.is_empty() {
            return hold;
        }
        self.replace_results(outgoing);
        // A buffered receipt is never counted: it leaves a later block, with
        // its creator's outcome in an earlier one, so its transaction lists
        // it apart. A hold, or a fault whose selector has an `nth` (which
        // counts only receipts it fits), passes it over; any other fault
        // panics, as it can't reach it.
        for r in buffered {
            let Some((tx, f)) =
                root.get(r.receipt_id()).and_then(|t| Some((t, self.faults.get_mut(t)?)))
            else {
                continue;
            };
            assert!(
                f.lenient
                    || f.injections.iter().all(|(at, f)| f.is_hold() || at.nth_of().is_some()),
                "congestion control held back receipt {} of transaction {tx}, which has faults: \
                 they apply only to receipts that leave the block that creates them",
                r.receipt_id(),
            );
            f.buffered.push(Spawned::leaving(r, outgoing, &producer));
        }
        let (mut due, mut until) = (vec![], vec![]);
        for (j, r) in outgoing.iter().enumerate() {
            let Some(&tx) = root.get(r.receipt_id()) else { continue };
            let Some(f) = self.faults.get_mut(&tx) else { continue };
            if own.contains(r.receipt_id()) || r.predecessor_id().is_system() {
                continue;
            }
            let Some(inputs) = input_data_ids(r) else { continue };
            let s = Spawned::leaving(r, outgoing, &producer);
            let n = f.spawned.len();
            let [run, late, result] = f.select(n, &s);
            let site = s.site();
            if let Some(i) = run {
                let (at, fault) = f.injections[i].clone();
                due.push(Due {
                    tx,
                    i,
                    j,
                    at,
                    fault,
                    site: site.clone(),
                    inputs,
                    lenient: f.lenient,
                });
            }
            if let Some(i) = late {
                match f.injections[i].1 {
                    Fault::Hold(blocks) => drop(hold.insert(s.id, Wait::Blocks(blocks))),
                    // Once the block's receipts are all spawned: the one it
                    // waits for may leave after it.
                    _ => until.push((tx, i, s.id, site.clone())),
                }
                f.won.push((i, s.id));
            }
            if let Some(i) = result {
                let Fault::HoldResult(blocks) = f.injections[i].1 else { unreachable!("a hold") };
                // A hold that can't apply panics, as a fault does, unless
                // the transaction is lenient (an explored step's injection).
                if s.sends {
                    f.result_holds.insert(n, blocks);
                    f.won.push((i, s.id));
                } else {
                    assert!(
                        f.lenient,
                        "{:?} at {} ({site}): its result goes to no callback, so there is \
                         nothing to hold (hold the receipt itself with Fault::Hold)",
                        f.injections[i].1, f.injections[i].0
                    );
                }
            }
            for d in output_data_ids(r) {
                f.results.entry(d).or_default().push(n);
            }
            f.spawned.push(s);
        }
        // The data receipts that carry those results, to any transaction's
        // callback: held if a `HoldResult` holds a receipt that sends it.
        for r in outgoing.iter() {
            let ReceiptEnum::Data(d) = r.receipt() else { continue };
            for f in self.faults.values_mut() {
                let Some(senders) = f.results.get(&d.data_id) else { continue };
                if let Some(&blocks) = senders.iter().find_map(|k| f.result_holds.get(k)) {
                    hold.insert(*r.receipt_id(), Wait::Blocks(blocks));
                }
            }
        }
        // A `HoldUntil` waits for a receipt that hasn't run, nor is this one.
        for (tx, i, id, site) in until {
            let Fault::HoldUntil(at) = &self.faults[&tx].injections[i].1 else {
                unreachable!("a HoldUntil")
            };
            // A snapshot keeps the log, not what each transaction spawned,
            // and is taken settled.
            let named = at.tx_of().filter(|t| !self.recorded(t));
            let target = self.target(&tx, i);
            let ran = target.is_some_and(|t| {
                self.log.position(&t).is_some() || executed.iter().any(|o| o.id == t)
            });
            let f = self.faults.get_mut(&tx).expect("its faults");
            match target {
                Some(t) if t == id => f.refuse(i, id, &site, "it would wait for itself"),
                Some(t) if ran => {
                    f.refuse(i, id, &site, &format!("receipt {t}, which it waits for, already ran"))
                }
                _ if let Some(t) = named => {
                    let why = format!(
                        "transaction {t}, a receipt of which it waits for, settled before the \
                         sim was loaded from a snapshot (receipts of a yield it parked aren't \
                         counted): it already ran"
                    );
                    f.refuse(i, id, &site, &why)
                }
                _ => drop(hold.insert(id, Wait::Until { tx, i })),
            }
        }
        // Each fault applies, giving the receipts it starved, or says why
        // it can't.
        let calls = |r: &Receipt| function_calls(r).len();
        for Due { tx, i, j, at, fault, site, inputs, lenient } in due {
            let id = *outgoing[j].receipt_id();
            let starved = match &fault {
                Fault::OutOfGas => self.starve(j, None, executed, &creator, outgoing),
                Fault::Gas(g) => match calls(&outgoing[j]) {
                    1 => self.starve(j, Some(*g), executed, &creator, outgoing),
                    n => Err(format!("Fault::Gas needs one function call, and it makes {n}")),
                },
                Fault::PromiseFailed => {
                    let k: Option<Vec<usize>> = inputs
                        .iter()
                        .map(|d| producer.get(d).copied().filter(|&k| calls(&outgoing[k]) > 0))
                        .collect();
                    match k {
                        _ if inputs.is_empty() => Err("it waits on no promise".to_string()),
                        None => Err("a call it waits on left an earlier block or makes no \
                                     function call, so nothing can fail it"
                            .to_string()),
                        Some(ks) => {
                            for k in ks {
                                let id = *outgoing[k].receipt_id();
                                // It makes a call, and 1 gas a call is the
                                // least any attaches: nothing to refuse.
                                let starved = self.starve(k, None, executed, &creator, outgoing);
                                starved.expect("lowering a call's gas");
                                self.log.faulted.entry(id).or_insert(Fault::OutOfGas);
                            }
                            Ok(())
                        }
                    }
                }
                Fault::ReturnValue(_) if inputs.is_empty() => {
                    Err("it waits on no promise".to_string())
                }
                Fault::ReturnValue(v) if !v.is_empty() && no_call(&inputs, outgoing, &producer) => Err(
                    "a receipt it waits on makes no function call, so its result is always empty"
                        .to_string(),
                ),
                Fault::ReturnValue(v) => {
                    let f = self.faults.get_mut(&tx).expect("its faults");
                    f.returns.extend(inputs.iter().map(|d| (*d, v.clone())));
                    Ok(())
                }
                Fault::Hold(_) | Fault::HoldUntil(_) | Fault::HoldResult(_) => {
                    unreachable!("holds aren't run faults")
                }
            };
            match starved {
                Ok(()) => {
                    self.log.faulted.entry(id).or_insert(fault);
                    self.faults.get_mut(&tx).expect("its faults").won.push((i, id));
                }
                Err(why) if lenient => {
                    let f = self.faults.get_mut(&tx).expect("its faults");
                    f.passed.push((fault, format!("{site}: {why}")));
                }
                Err(why) => panic!("{fault:?} at {at} ({site}): {why}"),
            }
        }
        drop_empty_refunds(executed, outgoing);
        hold
    }

    /// Replaces the successful results `ReturnValue` asked for, in the data
    /// receipts that carry them.
    fn replace_results(&mut self, outgoing: &mut [Receipt]) {
        for r in outgoing {
            let ReceiptEnum::Data(d) = r.receipt_mut() else { continue };
            let mut faults = self.faults.values_mut();
            if let Some(v) = faults.find_map(|f| f.returns.remove(&d.data_id))
                && d.data.is_some()
            {
                d.data = Some(v);
            }
        }
    }

    /// Sets the gas of receipt `j`'s calls to `gas` (`None`: each to the
    /// least a receipt can attach, 1, as no call attaches 0), and moves
    /// what that frees, at the price the gas was bought, into the gas
    /// refund its creator sends its signer in this block, adding one if
    /// there is none. Gas added comes out of that refund; more than it
    /// holds is refused, and changes nothing. A refund left empty is
    /// dropped once the block's faults are applied.
    fn starve(
        &mut self,
        j: usize,
        gas: Option<Gas>,
        executed: &mut [ExecutionOutcomeWithId],
        creator: &HashMap<CryptoHash, usize>,
        outgoing: &mut Vec<Receipt>,
    ) -> Result<(), String> {
        let id = *outgoing[j].receipt_id();
        let fcs: Vec<Gas> = function_calls(&outgoing[j]).into_iter().map(|c| c.1).collect();
        if fcs.is_empty() {
            return Err("it makes no function call".into());
        }
        let before = fcs.iter().fold(Gas::ZERO, |s, g| s.saturating_add(*g));
        let each = gas.unwrap_or(Gas::from_gas(1));
        let after = Gas::from_gas(each.as_gas().saturating_mul(fcs.len() as u64));
        let a = action_receipt(&outgoing[j]).expect("a receipt that runs calls");
        let (signer, key, price) =
            (a.signer_id().clone(), a.signer_public_key().clone(), a.gas_price());
        let yocto = |g: Gas| u128::from(g.as_gas()) * price.as_yoctonear();
        let freed = yocto(before.saturating_sub(after));
        let taken = yocto(after.saturating_sub(before));
        let parent = &mut executed[creator[&id]];
        let refund =
            parent.outcome.receipt_ids.iter().find_map(|c| {
                outgoing.iter().position(|o| o.receipt_id() == c && is_gas_refund(o))
            });
        let had = refund.map_or(0, |k| refund_deposit(&mut outgoing[k]).as_yoctonear());
        if had < taken {
            return Err(format!(
                "Fault::Gas adds {taken} yocto of gas to receipt {id}, but its creator had {had} \
                 yocto of unused gas to refund: no creator could have attached it"
            ));
        }
        // nearcore 2.14 makes every receipt a contract creates `ActionV2`,
        // and a delegate's inner one a first-version `Action`.
        let actions = match outgoing[j].receipt_mut() {
            ReceiptEnum::Action(a) => &mut a.actions,
            ReceiptEnum::ActionV2(a) => &mut a.actions,
            _ => unreachable!("a receipt that runs calls is an action receipt"),
        };
        set_gas(actions, each);
        let refund = refund.unwrap_or_else(|| {
            let Receipt::V0(mut r) = Receipt::new_gas_refund(&signer, Balance::ZERO, key);
            r.receipt_id = hash(&[id.as_ref(), b"fault refund"].concat());
            parent.outcome.receipt_ids.push(r.receipt_id);
            outgoing.push(Receipt::V0(r));
            outgoing.len() - 1
        });
        *refund_deposit(&mut outgoing[refund]) = Balance::from_yoctonear(had - taken + freed);
        Ok(())
    }
}

/// What a gas refund pays back.
fn refund(r: &Receipt) -> Balance {
    let a = action_receipt(r).expect("a gas refund is an action receipt");
    let [Action::Transfer(TransferAction { deposit })] = a.actions() else {
        unreachable!("a gas refund is one transfer")
    };
    *deposit
}

/// What a gas refund pays back, to change.
fn refund_deposit(r: &mut Receipt) -> &mut Balance {
    let ReceiptEnum::Action(a) = r.receipt_mut() else {
        unreachable!("a gas refund is a first-version action receipt")
    };
    let [Action::Transfer(TransferAction { deposit })] = &mut a.actions[..] else {
        unreachable!("a gas refund is one transfer")
    };
    deposit
}

/// The function calls of an action receipt, by method and gas; none for
/// any other receipt.
fn function_calls(r: &Receipt) -> Vec<(String, Gas)> {
    let fcs = action_receipt(r)
        .map(|a| calls(a.actions()).map(|c| (c.method_name.clone(), c.gas)).collect());
    fcs.unwrap_or_default()
}

/// The kinds of an action receipt's actions, nearcore's variant names;
/// none for any other.
fn action_kinds(r: &Receipt) -> Vec<String> {
    let kinds = action_receipt(r).map(|a| a.actions().iter().map(|a| a.as_ref().into()).collect());
    kinds.unwrap_or_default()
}

/// The promise results an action receipt waits on; `None` for any other.
fn input_data_ids(r: &Receipt) -> Option<Vec<CryptoHash>> {
    action_receipt(r).map(|a| a.input_data_ids().to_vec())
}

/// The data ids an action receipt's result goes to.
fn output_data_ids(r: &Receipt) -> Vec<CryptoHash> {
    let receivers = action_receipt(r).map(|a| a.output_data_receivers().to_vec());
    receivers.unwrap_or_default().iter().map(|d| d.data_id).collect()
}

fn set_gas(actions: &mut [Action], gas: Gas) {
    for a in actions {
        if let Action::FunctionCall(fc) = a {
            fc.gas = gas;
        }
    }
}
