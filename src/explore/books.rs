//! [`Books`]: what a contract owes and what it holds, declared once; the
//! checks follow from it ([`Harness::books`]).
//!
//! Every derived check is an ordinary named check; the skill's harness.md
//! lists each and what fails it. Value checks compare the world at rest with
//! the world before the case's first op (the fixture, or a curve's snapshot),
//! so ops in flight together are judged exactly; a row's `broke` names
//! the op.
//!
//! Liveness plays each open claim's redeem plan from where the case left
//! it, within [`LIVENESS_EPOCHS`]; a claim it can't reach is not judged.

use super::check::{End, Harness, OpDone, Player};
use super::run::merge;
use super::{Fail, Op, Plan, StepGas, World};
use crate::findings::guarded;
use crate::{Delta, TxOutcome};
use near_primitives::hash::CryptoHash;
use near_primitives::types::AccountId;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::rc::Rc;

/// The most epochs liveness advances to a claim's maturity.
pub const LIVENESS_EPOCHS: u64 = 8;

type Read<W> = Rc<dyn Fn(&W) -> BTreeMap<String, u128>>;
type Held<W> = Rc<dyn Fn(&W) -> u128>;
/// A reading of the books at rest, for [`Harness::diff`].
pub(crate) type Reading<W> = Held<W>;
type OpenFn<W> = Rc<dyn Fn(&W) -> Vec<Claim>>;
type RedeemFn<O> = Rc<dyn Fn(&Claim) -> Plan<O>>;
type KeyFn<O, W> = Rc<dyn for<'a> Fn(&OpDone<'a, O, W>) -> Option<String>>;
type AuthFn<O, W> = Rc<dyn for<'a> Fn(&OpDone<'a, O, W>) -> Option<Vec<String>>>;

/// One open claim on the contract: `holder` may redeem `amount` from
/// epoch `matures` on (`None`: now).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Claim {
    /// Who holds it.
    pub holder: String,
    /// What it is worth, in its asset's unit.
    pub amount: u128,
    /// The epoch height it matures at, if not now.
    pub matures: Option<u64>,
}

/// An asset the contract holds.
struct Asset<W> {
    name: String,
    held: Held<W>,
    /// NEAR of this account: a case's change is its ops' moves, gas
    /// rewards and gas left out.
    near: Option<AccountId>,
    /// A reading only ([`Books::watch_near`]): no check is derived from it.
    watched: bool,
    /// How it was declared, `watch_near("a.near")`: a name taken twice
    /// names both.
    decl: String,
}

struct ClaimDecl<O, W> {
    name: String,
    asset: String,
    open: OpenFn<W>,
    redeem: RedeemFn<O>,
}

/// A contract's books: its liabilities, the assets backing them, its
/// claims and its one-time keys. [`Harness::books`] derives the checks.
pub struct Books<O, W = crate::Sim> {
    liabilities: Vec<(String, Read<W>)>,
    assets: Vec<Asset<W>>,
    backs: Vec<(String, String)>,
    owned: Vec<String>,
    prices: Vec<(String, String)>,
    claims: Vec<ClaimDecl<O, W>>,
    once: Vec<(String, KeyFn<O, W>)>,
    at_end: bool,
    authorized: Option<AuthFn<O, W>>,
    tolerances: BTreeMap<String, u128>,
    supplies: Vec<(String, Held<W>)>,
}

impl<O: Op<W>, W: World> Default for Books<O, W> {
    fn default() -> Self {
        Books {
            liabilities: vec![],
            assets: vec![],
            backs: vec![],
            owned: vec![],
            prices: vec![],
            claims: vec![],
            once: vec![],
            at_end: false,
            authorized: None,
            tolerances: BTreeMap::new(),
            supplies: vec![],
        }
    }
}

impl<O: Op<W>, W: World> Books<O, W> {
    /// No books yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// A liability: what the contract owes, per holder, as `read` finds it
    /// in the world (a view, a state read; an internal ledger's balances
    /// of one id, such as `nep141:wrap.near` held inside the contract).
    ///
    /// `read` must list every holder: read them from the contract (an
    /// enumerating view, its events, its state) rather than a list by
    /// hand. A holder it misses reads as kept, owed to no one, so declare
    /// the contract's stated total with [`supply`](Self::supply), which
    /// fails loudly then.
    pub fn liability(
        mut self,
        name: &str,
        read: impl Fn(&W) -> BTreeMap<String, u128> + 'static,
    ) -> Self {
        self.liabilities.push((name.to_owned(), Rc::new(read)));
        self
    }

    /// An asset: what the contract holds, as `held` reads it (a token
    /// balance, stake). For the contract's own NEAR use [`near`](Self::near).
    pub fn asset(mut self, name: &str, held: impl Fn(&W) -> u128 + 'static) -> Self {
        let (decl, held) = (format!("asset({name:?}, ..)"), Rc::new(held));
        self.assets.push(Asset { name: name.to_owned(), held, near: None, watched: false, decl });
        self
    }

    /// The NEAR `account` holds, locked (staked) included, as an asset. Its
    /// change in a case is what moved in and out, and the epoch rewards
    /// its stake was paid ([`Sim::rewarded`](crate::Sim::rewarded)), gas
    /// left out.
    ///
    /// 1-yocto proofs (a call's proof of a full-access key) are exempt both
    /// ways, counted per call: each move of exactly 1 yocto into or out of
    /// `account` is left out, and allows the kept and backing checks 1
    /// yocto either way, for a proof returned inside a larger payment (as
    /// wrap.near's `near_withdraw` pays `amount + 1`), or paid on from one.
    /// The blind spot: a call that moves a proof hides any 1-yocto
    /// discrepancy, a leak of 1 yocto included. A receipt `account` sends
    /// itself (a call it signs, a promise to itself) moves no NEAR in or
    /// out, so its yocto is no proof: a contract that books it owes 1
    /// yocto it never received, "1 unbacked". Sign an admin op from the
    /// admin's own account, as on a real network; otherwise the drift is
    /// the contract's.
    ///
    /// Rewards paid to the stake but not yet booked by the contract (a
    /// liquid-staking pool that syncs them later) read as kept, owed to no
    /// one, until it syncs: book them as a liability of their own, one
    /// holder `"unsynced rewards"`, backed by the same asset
    /// ([`backs`](Self::backs)): [`Sim::rewarded`](crate::Sim::rewarded) since the fixture
    /// less the rewards the contract has booked since. Not a holder of a
    /// liability that [`owned`](Self::owned), [`supply`](Self::supply) or
    /// [`price`](Self::price) reads: it falls at a sync no holder signs. Never book the balance less
    /// the contract's tracker: that holder absorbs stray NEAR, the
    /// in-flight double count the books exist to catch.
    pub fn near(self, name: &str, account: &str) -> Self {
        self.near_asset(name, account, false, format!("near({name:?}, {account:?})"))
    }

    /// `account`'s NEAR as a reading only: [`SERIALIZABLE`] compares it,
    /// net of gas and with the stake's rewards, as [`near`](Self::near)
    /// reads it; [`Harness::diff`] shows its balance, gas included. No
    /// kept, backing or `unjudged:` item is derived. For a plain account
    /// an interleaving can move (a caller paid twice); a custodian's NEAR
    /// is `near`. Its name is `"<account> NEAR"`, for
    /// [`tolerance`](Self::tolerance), and no other asset may take it;
    /// [`Harness::books`] panics on any rule naming it
    /// ([`backs`](Self::backs), [`price`](Self::price), a
    /// [`claim`](Self::claim)).
    ///
    /// [`SERIALIZABLE`]: super::builtin::SERIALIZABLE
    pub fn watch_near(self, account: &str) -> Self {
        let decl = format!("watch_near({account:?})");
        self.near_asset(&format!("{account} NEAR"), account, true, decl)
    }

    fn near_asset(mut self, name: &str, account: &str, watched: bool, decl: String) -> Self {
        let id: AccountId = account.parse().expect("an account id");
        let of = id.clone();
        let held = move |w: &W| {
            let a = w.as_ref().account(&of);
            a.map_or(0, |a| a.amount.saturating_add(a.locked).as_yoctonear())
        };
        let (name, held, near) = (name.to_owned(), Rc::new(held), Some(id));
        self.assets.push(Asset { name, held, near, watched, decl });
        self
    }

    /// `account` owes no one NEAR: any NEAR it gains in a case, beyond
    /// what [`near`](Self::near) leaves out or allows, is kept ("`account`
    /// NEAR is kept, owed to no one"). A contract that pays NEAR out as it
    /// comes in. The asset is named `"<account> NEAR"`: that is the name
    /// [`tolerance`](Self::tolerance) takes, `.tolerance("intents.near
    /// NEAR", n)`.
    ///
    /// A contract that keeps NEAR for the storage a caller adds is judged
    /// per op instead, by the storage recipe in the skill's
    /// [harness.md](https://github.com/near/near-contract-sim/blob/main/skills/near-contract-sim/references/harness.md):
    /// three [`per_op`](Harness::per_op) checks comparing
    /// `effects().near_net(contract)` with
    /// [`Sim::storage_cost`](crate::Sim::storage_cost) of
    /// the bytes `effects().storage_bytes(contract)` added, and [`Op::refused`].
    pub fn keeps_no_near(self, account: &str) -> Self {
        let decl = format!("keeps_no_near({account:?})");
        self.near_asset(&format!("{account} NEAR"), account, false, decl)
    }

    /// `asset` backs `liability` one for one, the liability allowed to
    /// exceed it by the asset's [`tolerance`](Self::tolerance).
    pub fn backs(mut self, liability: &str, asset: &str) -> Self {
        self.backs.push((liability.to_owned(), asset.to_owned()));
        self
    }

    /// Each holder's amount of `liability` falls only where the holder
    /// signed an op: nobody else spends it. Her amount is read at the
    /// case's start and before each op submitted at rest (nothing in
    /// flight or parked), so one who appears in the case counts. It fails
    /// where at rest it is below her highest reading and she signed no op
    /// but those that raised it (an op between two readings where hers
    /// rose): a deposit is not consent to be drained. Any other op she
    /// signed covers any fall, as an approve covers a later pull. Blind
    /// spots: an op of hers that changed nothing (a registration) covers a
    /// later drain, unless [`authorized`](Self::authorized) names no one
    /// for it; and under overlap fewer ops submit at rest, so a window
    /// where hers rose can hold an approve, which then doesn't count: a
    /// false finding. Idle ([`Fail::idle`]) while every reading is zero.
    pub fn owned(mut self, liability: &str) -> Self {
        self.owned.push(liability.to_owned());
        self
    }

    /// `liability` is shares of `asset`: the asset per share never falls
    /// below its first reading with shares outstanding (at the case's
    /// start, else before an op submitted at rest, nothing in flight or
    /// parked: an empty pool's first deposit), by more than one unit of
    /// rounding. Idle ([`Fail::idle`]) with no such reading, or no shares
    /// outstanding at rest.
    pub fn price(mut self, liability: &str, asset: &str) -> Self {
        self.prices.push((liability.to_owned(), asset.to_owned()));
        self
    }

    /// Claims on `asset` that `open` lists, each redeemed by the plan
    /// `redeem` gives: liveness ([`LIVENESS_EPOCHS`]).
    pub fn claim(
        mut self,
        name: &str,
        asset: &str,
        open: impl Fn(&W) -> Vec<Claim> + 'static,
        redeem: impl Fn(&Claim) -> Plan<O> + 'static,
    ) -> Self {
        let (name, asset, open, redeem) =
            (name.into(), asset.into(), Rc::new(open), Rc::new(redeem));
        self.claims.push(ClaimDecl { name, asset, open, redeem });
        self
    }

    /// Who authorized each op, for [`owned`](Self::owned): by default its
    /// transactions' signers. A relayed op (a meta transaction, a payload
    /// signed in its args) names its holder here; `None` keeps the default.
    /// A raw call a repeat or a tail copies from an op is that op's copy,
    /// [`OpDone::op`] the copy.
    pub fn authorized(
        mut self,
        by: impl for<'a> Fn(&OpDone<'a, O, W>) -> Option<Vec<String>> + 'static,
    ) -> Self {
        self.authorized = Some(Rc::new(by));
        self
    }

    /// Differences up to `units` of the asset or liability `name` are
    /// rounding, not findings: an asset's in its kept, backing and backs
    /// checks, a liability's in its owned and supply checks. 0 by default.
    /// `name` is the one given to [`asset`](Self::asset),
    /// [`near`](Self::near) or [`liability`](Self::liability) (for
    /// [`keeps_no_near`](Self::keeps_no_near), `"<account> NEAR"`);
    /// [`Harness::books`] panics on one that names neither.
    ///
    /// The slack is one bound per check (per holder for `owned`), never
    /// per op: drift that builds up over a case's ops counts against it
    /// whole, so a case drifting 1 unit at each of `units` ops still
    /// passes. Keep it to real rounding; never widen it to quiet 1-yocto
    /// proofs (`near` already leaves those out), or it hides a leak of a
    /// yocto per call.
    pub fn tolerance(mut self, name: &str, units: u128) -> Self {
        self.tolerances.insert(name.to_owned(), units);
        self
    }

    /// `supply` reads `liability`'s total as the contract states it: at
    /// rest it equals the sum of the holders the liability reads, within
    /// the tolerance. A holder the reader misses fails it, where it would
    /// otherwise read only as kept, owed to no one.
    pub fn supply(mut self, liability: &str, supply: impl Fn(&W) -> u128 + 'static) -> Self {
        self.supplies.push((liability.to_owned(), Rc::new(supply)));
        self
    }

    /// Judges every derived at-rest check once, at the case's end
    /// ([`Harness::at_end`]), not at each rest point: cheaper for costly
    /// views, and for epoch gaps, whose every idle block is a rest point
    /// judged in full.
    pub fn at_end(mut self) -> Self {
        self.at_end = true;
        self
    }

    /// A key each complete op consumes at most once in a case (a nonce, a
    /// claim id): `key` gives the one it consumed, if any. A failed op is
    /// complete too: give `None` for one whose outcome failed. A raw call
    /// a repeat or a tail copies from an op is judged as that op
    /// ([`OpDone::op`] the copied op), so read the key from what the call
    /// sent, `d.entries().first()?.args_json::<Value>().ok()` (an op with
    /// no transaction has none), not from the op value: a copy may carry
    /// another payload, or come from another caller. A plain
    /// [`Plan::call`] is not judged.
    pub fn once(
        mut self,
        name: &str,
        key: impl for<'a> Fn(&OpDone<'a, O, W>) -> Option<String> + 'static,
    ) -> Self {
        self.once.push((name.to_owned(), Rc::new(key)));
        self
    }
}

impl<O: Op<W>, W: World> Harness<O, W> {
    /// The checks `books` implies, each named as the skill's harness.md
    /// lists them. Each reader runs on the fixture first, and the backing
    /// checks run there with the liability one unit past the asset: a
    /// reader that panics, or a check that doesn't fire then, panics here.
    pub fn books(mut self, books: Books<O, W>) -> Self {
        let (fixture, b) = (self.fixture.clone(), books);
        let (liabilities, assets, backs, prices, tolerances) =
            (b.liabilities, b.assets, b.backs, b.prices, b.tolerances);
        for (i, a) in assets.iter().enumerate() {
            if let Some(b) = assets[i + 1..].iter().find(|b| b.name == a.name) {
                panic!(
                    "Books: two assets named {:?}, {} and {}: a rule naming it reads one, and \
                     they share a tolerance; name each once",
                    a.name, a.decl, b.decl
                );
            }
        }
        for name in tolerances.keys() {
            let known = liabilities.iter().any(|(n, _)| n == name)
                || assets.iter().any(|a| a.name == *name);
            assert!(known, "Books: tolerance({name:?}, ..) names no asset or liability");
        }
        let tol = |name: &str| tolerances.get(name).copied().unwrap_or(0);
        let first = self.at_rest.len();
        let read = |name: &str| -> Read<W> {
            let found = liabilities.iter().find(|(n, _)| n == name);
            found.unwrap_or_else(|| panic!("Books: no liability {name:?}")).1.clone()
        };
        let asset = |name: &str| -> &Asset<W> {
            let found = assets.iter().find(|a| a.name == name);
            let a = found.unwrap_or_else(|| panic!("Books: no asset {name:?}"));
            assert!(
                !a.watched,
                "Books: {name:?} is a reading (watch_near), not an asset: use near"
            );
            a
        };
        let total = |m: BTreeMap<String, u128>| m.values().fold(0u128, |t, v| t.saturating_add(*v));
        for (name, lr) in &liabilities {
            let lr = lr.clone();
            let owed: Reading<W> = Rc::new(move |w: &W| total(lr(w)));
            self.readings.push((format!("{name} owed"), owed));
        }
        for a in &assets {
            self.readings.push((format!("{} held", a.name), a.held.clone()));
        }
        let on_fixture = |what: &str, f: &dyn Fn() -> u128| {
            guarded(f).unwrap_or_else(|p| panic!("Books: {what} on the fixture: {p}"))
        };
        // Backing, its control, and the case's net change each way.
        let mut by_asset: BTreeMap<String, Vec<Read<W>>> = BTreeMap::new();
        for (l, a) in backs.clone() {
            let slack = tol(&a);
            let (lr, held) = (read(&l), asset(&a).held.clone());
            let has = on_fixture(&a, &|| held(&fixture));
            let owed = on_fixture(&l, &|| total(lr(&fixture)));
            by_asset.entry(a.clone()).or_default().push(lr.clone());
            let name = format!("{l} is backed by {a}");
            let backing = move |w: &W, skew: u128| {
                let (owed, has) = (total(lr(w)).saturating_add(skew), held(w));
                if owed == 0 {
                    return Err(Fail::idle(format!("{l} owes nothing")));
                }
                let why = || Fail::new(format!("{l} total {owed}, {a} holds {has}"));
                (owed <= has.saturating_add(slack)).then_some(()).ok_or_else(why)
            };
            // The control: one unit past the asset, it fires.
            let skew = has.saturating_add(slack).saturating_add(1).saturating_sub(owed);
            assert!(backing(&fixture, skew).is_err(), "Books: `{name}` didn't fire past its asset");
            self = self.at_rest(&name, move |w: &W| backing(w, 0));
        }
        for a in assets.iter().filter(|a| !a.watched) {
            let liabilities = by_asset.remove(&a.name).unwrap_or_default();
            on_fixture(&a.name, &|| (a.held)(&fixture));
            let (held, near, name) = (a.held.clone(), a.near.clone(), a.name.clone());
            let (tol, a_name) = (tol(&name), name.clone());
            // Since the case's start: the world before its first op; idle if
            // the asset and what it backs read zero at both ends.
            // (owed, held, proofs): the change in what it backs and in what
            // it holds since the case's start, and its 1-yocto proofs.
            let net =
                Rc::new(move |w: &W, ops: &[OpDone<O, W>]| -> Result<(Delta, Delta, u128), Fail> {
                    let start = ops.first().map_or(w, |d| d.before());
                    let empty =
                        |w: &W| held(w) == 0 && liabilities.iter().all(|r| total(r(w)) == 0);
                    if near.is_none() && empty(w) && empty(start) {
                        return Err(Fail::idle(format!("{a_name} never held anything")));
                    }
                    let owed: Delta = liabilities
                        .iter()
                        .map(|r| Delta::between(total(r(start)), total(r(w))))
                        .sum();
                    let (held, proofs) = match &near {
                        Some(id) => {
                            let reward = |w: &W| w.as_ref().rewarded(id).as_yoctonear();
                            // 1-yocto proofs either way are left out, each
                            // counted.
                            let proofs = || {
                                (ops.iter().flat_map(|d| &d.effects().near)).filter(|m| {
                                    m.from != m.to
                                        && m.amount.as_yoctonear() == 1
                                        && (m.to == *id || m.from == *id)
                                })
                            };
                            let yocto = |m: &crate::NearMove| {
                                if m.to == *id { Delta::gain(1) } else { Delta::loss(1) }
                            };
                            let moved: Delta = ops.iter().map(|d| d.effects().near_net(id)).sum();
                            let exempt: Delta = proofs().map(yocto).sum();
                            let reward = Delta::between(reward(start), reward(w));
                            (moved - exempt + reward, proofs().count() as u128)
                        }
                        None => (Delta::between(held(start), held(w)), 0),
                    };
                    Ok((owed, held, proofs))
                });
            let n = net.clone();
            let kept = format!("{name} is kept, owed to no one");
            let backs_any = backs.iter().any(|(_, x)| *x == a.name);
            let priced = prices.iter().any(|(_, x)| *x == a.name);
            // A price-only asset grows by design: no one is owed it.
            if !priced || backs_any {
                self = self.at_rest_ops(&kept, move |w: &W, ops: &[OpDone<O, W>]| {
                    let (owed, held, proofs) = n(w, ops)?;
                    let kept = held - owed;
                    let mut why = format!("since the case began {name} changed by {held}, what it backs by {owed}: {kept} kept, owed to no one{}", allowed(proofs));
                    if backs_any {
                        why += ", or held for a holder the liability reader misses: `.supply` tells them apart";
                    }
                    (kept <= Delta::gain(tol.saturating_add(proofs))).then_some(()).ok_or(Fail::new(why))
                });
            }
            if backs_any {
                let name = a.name.clone();
                self = self.at_rest_ops(&format!("{name} backs what it owes"), move |w, ops| {
                    let (owed, held, proofs) = net(w, ops)?;
                    let short = owed - held;
                    let why = format!("since the case began what {name} backs grew by {owed}, {name} by {held}: {short} unbacked{}", allowed(proofs));
                    (short <= Delta::gain(tol.saturating_add(proofs))).then_some(()).ok_or(Fail::new(why))
                });
            }
        }
        for l in &b.owned {
            let (lr, l, auth, tol) = (read(l), l.clone(), b.authorized.clone(), tol(l));
            self = self.at_rest_ops(&format!("{l} falls only for its holder"), move |w, ops| {
                let idle = || Fail::idle(format!("no holder held {l} at any reading"));
                // The readings at the case's start and before each op
                // submitted at rest: nothing in flight or parked.
                let readings: Vec<(usize, BTreeMap<String, u128>)> = (ops.iter().enumerate())
                    .filter(|(i, d)| *i == 0 || d.rest)
                    .map(|(i, d)| (i, lr(d.before())))
                    .collect();
                if readings.iter().all(|(_, r)| r.values().all(|v| *v == 0)) {
                    return Err(idle());
                }
                // A raw call copying an op (a repeat's or a tail's) is
                // that op's: its signers authorized it.
                let by = |d: &OpDone<O, W>| -> Vec<String> {
                    let d = &OpDone { op: d.op.or(d.of), ..d.clone() };
                    let named = d.op.and(auth.as_ref()).and_then(|f| f(d));
                    let entries =
                        || d.entries().iter().map(|r| r.predecessor_id.to_string()).collect();
                    named.unwrap_or_else(entries)
                };
                let signed: Vec<Vec<String>> = ops.iter().map(by).collect();
                let (now, of) =
                    (lr(w), |r: &BTreeMap<String, u128>, h: &str| r.get(h).copied().unwrap_or(0));
                let holders: BTreeSet<&String> =
                    readings.iter().flat_map(|(_, r)| r.keys()).collect();
                for holder in holders {
                    let was = readings.iter().map(|(_, r)| of(r, holder)).max().unwrap_or(0);
                    let is = of(&now, holder);
                    if is.saturating_add(tol) >= was {
                        continue;
                    }
                    // The ops between two readings where hers rose gave
                    // her more, and authorize nothing: a deposit is not
                    // consent to be drained.
                    let gave: Vec<std::ops::Range<usize>> = (readings.windows(2))
                        .filter(|p| of(&p[1].1, holder) > of(&p[0].1, holder))
                        .map(|p| p[0].0..p[1].0)
                        .collect();
                    let mine = |(i, s): (usize, &Vec<String>)| {
                        !gave.iter().any(|g| g.contains(&i)) && s.contains(holder)
                    };
                    if !signed.iter().enumerate().any(mine) {
                        let but = if gave.is_empty() { "" } else { " but those that raised it" };
                        let why =
                            format!("{holder}'s {l} fell from {was} to {is}; it signed no op{but}");
                        return Err(Fail::new(why).key(holder.clone()));
                    }
                }
                Ok(())
            });
        }
        for (l, a) in &prices {
            let (lr, held, l) = (read(l), asset(a).held.clone(), l.clone());
            let name = format!("{l}'s price never falls");
            self = self.at_rest_ops(&name, move |w: &W, ops: &[OpDone<O, W>]| {
                let idle =
                    || Fail::idle(format!("no {l} outstanding when an op began, or at rest"));
                // The first reading with shares outstanding, at the case's
                // start or before an op submitted at rest: an empty pool's first
                // depositor is judged from her shares.
                let began = ops.iter().enumerate().filter(|(i, d)| *i == 0 || d.rest);
                let mut readings = began.map(|(_, d)| (total(lr(d.before())), held(d.before())));
                let (s0, a0) = readings.find(|(s, _)| *s > 0).ok_or_else(idle)?;
                let (s1, a1) = (total(lr(w)), held(w));
                if s1 == 0 {
                    return Err(idle());
                }
                // a1 / s1 >= a0 / s0, less one unit per share-set, in 256 bits.
                let (hi, lo) = wide(a1, s0);
                let lhs = (hi + u128::from(lo.checked_add(s0).is_none()), lo.wrapping_add(s0));
                let why = || Fail::new(format!("{a0} per {s0} shares fell to {a1} per {s1}"));
                (lhs >= wide(a0, s1)).then_some(()).ok_or_else(why)
            });
        }
        for (l, supply) in b.supplies {
            let (lr, tol) = (read(&l), tol(&l));
            on_fixture(&l, &|| supply(&fixture));
            self =
                self.at_rest(&format!("{l}'s supply is the sum of its holders"), move |w: &W| {
                    let (sum, stated) = (total(lr(w)), supply(w));
                    let why =
                        || Fail::new(format!("{l}'s holders sum to {sum}, its supply is {stated}"));
                    (sum.abs_diff(stated) <= tol).then_some(()).ok_or_else(why)
                });
        }
        for (name, key) in b.once {
            self = self.at_rest_ops(&format!("{name} at most once"), move |_, ops| {
                let mut seen = BTreeSet::new();
                // A raw call copying an op (a repeat's or a tail's) is that
                // op's: it can consume the op's key again.
                let judged = ops.iter().map(|d| OpDone { op: d.op.or(d.of), ..d.clone() });
                for k in judged.filter(|d| d.op.is_some()).filter_map(|d| key(&d)) {
                    if !seen.insert(k.clone()) {
                        return Err(Fail::new(format!("{name} {k} consumed twice")).key(k));
                    }
                }
                Ok(())
            });
        }
        for c in b.claims {
            let name = c.name.clone();
            let a = asset(&c.asset);
            let (held, near) = (a.held.clone(), a.near.clone());
            let ceiling = StepGas::Exactly(self.gas_ceiling());
            let check =
                move |w: &W, play: &Player<O, W>| liveness(&c, &held, &near, ceiling, w, play);
            let name = format!("{} is redeemable", name);
            self.end.push(End { name, check: Rc::new(check) });
        }
        self.at_rest[first..].iter_mut().for_each(|c| (c.at_end, c.calls) = (b.at_end, true));
        // Each books' readings add to the last's.
        let mut serial = self.serial.as_deref().cloned().unwrap_or_default();
        for (name, lr) in &liabilities {
            serial.liabilities.push((name.clone(), lr.clone(), tol(name)));
        }
        for a in &assets {
            serial.assets.push((a.name.clone(), a.held.clone(), a.near.clone(), tol(&a.name)));
        }
        self.serial = Some(Rc::new(serial));
        self
    }
}

/// What [`builtin::SERIALIZABLE`](super::builtin::SERIALIZABLE) reads: the
/// books' liabilities, per holder, and their assets, each with its
/// tolerance.
pub(crate) struct Serial<W> {
    liabilities: Vec<(String, Read<W>, u128)>,
    /// An asset's reading, its NEAR account if it is one, its tolerance.
    assets: Vec<(String, Held<W>, Option<AccountId>, u128)>,
}

impl<W> Clone for Serial<W> {
    fn clone(&self) -> Self {
        Serial { liabilities: self.liabilities.clone(), assets: self.assets.clone() }
    }
}

impl<W> Default for Serial<W> {
    fn default() -> Self {
        Serial { liabilities: vec![], assets: vec![] }
    }
}

/// The most ops one overlap judges: 3 make 6 serial orders.
pub(crate) const SERIAL_OPS: usize = 3;

/// One op of an overlap, as it ran in the case: its step, its name, and
/// its transactions' outcomes.
pub(crate) struct Ran<'a, O> {
    pub step: &'a super::Step<O>,
    pub name: &'a str,
    pub outcomes: Vec<TxOutcome>,
}

/// Ops in flight together: from one submitted at rest (`start`, the world
/// just before it) up to the next rest point (`end`, the world there).
/// `memo`, inside an enumeration, is its serial orders' memo and what
/// fixes `start`: the plan before the first of them (`run::start_key`).
pub(crate) struct Overlap<'a, O, W> {
    pub start: &'a W,
    pub end: &'a W,
    pub ops: Vec<Ran<'a, O>>,
    pub memo: Option<(&'a Orders, CryptoHash)>,
}

/// One serial order played: what each op sent, in play order, and, once
/// they sent what a case's ops did, which of them failed and the
/// readings; or why it didn't run.
type Played = Result<(Vec<Sent>, Option<(Vec<bool>, Readings)>), String>;

/// An enumeration's serial orders, each played once and shared by its
/// cases, by [`order_key`]. Only `Harness::enumerate` keeps one
/// ([`Memo`](super::check::Memo)): a scan, a search and `Harness::run`
/// play every order.
#[derive(Default)]
pub(crate) struct Orders(RefCell<HashMap<CryptoHash, Played>>);

impl Orders {
    fn get(&self, key: &CryptoHash) -> Option<Played> {
        self.0.borrow().get(key).cloned()
    }

    fn insert(&self, key: CryptoHash, played: Played) {
        self.0.borrow_mut().insert(key, played);
    }
}

#[cfg(test)]
thread_local! {
    /// Serial orders played on this thread: the memo's hits, seen.
    pub(crate) static PLAYED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// An enumeration plays every order, as before the memo: the
    /// differential tests' reference.
    pub(crate) static UNMEMOIZED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The memo key of the serial order `steps` of `o`: everything it reads.
/// - Its start, by `o.memo`'s hash of the plan that reached it
///   (`run::start_key`, which says why that fixes it): never the
///   enumeration's dedup key, which leaves out what an order can read
///   (the height within an epoch, accounts without code, a `W`'s own
///   fields).
/// - Its steps, in play order, as [`play_serial`] plays them: act, gas,
///   injections and stub answers by their serde (a replay's identity of
///   an op), each gap left out, as it plays them settled.
/// - The epoch it skips to, the one thing it reads of `o.end`.
/// - The books' readers are the harness's; an enumeration keeps one memo.
///
/// `None` outside an enumeration, or if a step doesn't serialize.
fn order_key<O: Op<W>, W: World>(
    o: &Overlap<'_, O, W>,
    steps: &[&super::Step<O>],
) -> Option<CryptoHash> {
    let (_, from) = o.memo?;
    let played: Vec<super::Step<O>> =
        steps.iter().map(|&s| super::Step { then: super::Gap::Settle, ..s.clone() }).collect();
    let bytes = serde_json::to_vec(&(from, played, o.end.as_ref().epoch_height())).ok()?;
    Some(CryptoHash::hash_bytes(&bytes))
}

/// A holder's reading: `liability: holder`, or an asset's change.
type Readings = BTreeMap<String, (Delta, u128)>;
/// A serial order run: the ops' order, its readings and which ops failed.
type Order = (Vec<usize>, Readings, Vec<bool>);

/// What one op sent, gas left out: each transaction's first receipts'
/// callers, receivers, methods, args and deposits.
type Sent = Vec<(AccountId, AccountId, Vec<String>, Vec<Vec<u8>>, near_primitives::types::Balance)>;

fn sent(outcomes: &[TxOutcome]) -> Sent {
    let first = |o: &TxOutcome| o.transaction.as_ref().map_or(vec![], |t| t.receipt_ids.clone());
    (outcomes.iter())
        .flat_map(|o| {
            let ids = first(o);
            (o.receipts.iter()).filter(move |r| ids.contains(&r.receipt_id))
        })
        .map(|r| {
            let (from, to) = (r.predecessor_id.clone(), r.receiver_id.clone());
            (from, to, r.method_names.clone(), r.args.clone(), r.deposit)
        })
        .collect()
}

/// Whether any of an op's transactions failed, or the op says the
/// contract refused it ([`Op::refused`]).
fn failed<O: Op<W>, W>(step: &super::Step<O>, outcomes: &[TxOutcome]) -> bool {
    outcomes.iter().any(|o| !o.is_success()) || step.act.op().is_some_and(|op| op.refused(outcomes))
}

/// Whether an op moved nothing besides gas: no NEAR between accounts, net,
/// and no storage written (a token moves only by its contract's storage).
fn moved_nothing(outcomes: &[TxOutcome]) -> bool {
    let e = merge(outcomes);
    let mut net: BTreeMap<&AccountId, i128> = BTreeMap::new();
    for m in &e.near {
        let a = m.amount.as_yoctonear() as i128;
        *net.entry(&m.from).or_default() -= a;
        *net.entry(&m.to).or_default() += a;
    }
    let receipts = || outcomes.iter().flat_map(|o| &o.receipts);
    net.values().all(|n| *n == 0) && receipts().all(|r| r.storage_ops.written.is_empty())
}

impl<W: World> Serial<W> {
    /// The readings of `end`, the world `outcomes` left after `start`: each
    /// liability per holder, and each asset's change: for NEAR, what moved
    /// in and out (gas left out) and the stake's rewards; for any other,
    /// its reading's change.
    fn read(&self, start: &W, end: &W, outcomes: &[TxOutcome]) -> Readings {
        let mut out = Readings::new();
        for (name, read, tol) in &self.liabilities {
            for (holder, v) in read(end) {
                out.insert(format!("{name}: {holder}"), (Delta::gain(v), *tol));
            }
        }
        let moved = merge(outcomes);
        for (name, held, near, tol) in &self.assets {
            let change = match near {
                Some(id) => {
                    let reward = |w: &W| w.as_ref().rewarded(id).as_yoctonear();
                    moved.near_net(id) + Delta::between(reward(start), reward(end))
                }
                None => Delta::between(held(start), held(end)),
            };
            out.insert(name.clone(), (change, *tol));
        }
        out
    }

    /// Judges each overlap: its readings must be what some serial order of
    /// the same ops gives, each run settled from the overlap's start. A
    /// failed op whose effects moved nothing may be left out of the order,
    /// if it also fails in some serial order; one that fails here and in
    /// no serial order is a finding (refused only under interleaving). An
    /// order that can't run, more than [`SERIAL_OPS`] ops, or an op that
    /// sends another call in an order (caller or arguments) is not judged.
    /// Inside an enumeration, an order plays once per [`order_key`]. If its
    /// ops sent other calls in it than in the case that first played it,
    /// its readings weren't taken: the first later case whose ops sent the
    /// same plays it again.
    pub(crate) fn judge<O: Op<W>>(&self, overlaps: &[Overlap<'_, O, W>]) -> Result<(), Fail> {
        let together: Vec<&Overlap<'_, O, W>> =
            overlaps.iter().filter(|o| o.ops.len() > 1).collect();
        if together.is_empty() {
            return Err(Fail::idle("no ops in flight together"));
        }
        let mut unjudged = None;
        for o in together {
            match self.overlap(o) {
                Err(f) if f.unjudged.is_none() => return Err(f),
                Err(f) => unjudged = unjudged.or(Some(f)),
                Ok(()) => {}
            }
        }
        unjudged.map_or(Ok(()), Err)
    }

    fn overlap<O: Op<W>>(&self, o: &Overlap<'_, O, W>) -> Result<(), Fail> {
        let n = o.ops.len();
        let names: Vec<&str> = o.ops.iter().map(|r| r.name).collect();
        if n > SERIAL_OPS {
            return Err(Fail::not_judged(format!(
                "{n} ops in flight together ({}): serial orders are tried for at most {SERIAL_OPS}",
                names.join(", ")
            )));
        }
        let all: Vec<TxOutcome> = o.ops.iter().flat_map(|r| r.outcomes.clone()).collect();
        let mine = self.read(o.start, o.end, &all);
        let order_name = |order: &[usize]| {
            let ops: Vec<&str> = order.iter().map(|&i| names[i]).collect();
            format!("[{}]", ops.join(", "))
        };
        // The first op of `order` that sent another call there than here.
        let cased: Vec<Sent> = o.ops.iter().map(|r| sent(&r.outcomes)).collect();
        let other = |order: &[usize], sent_by: &[Sent]| {
            order.iter().zip(sent_by).position(|(&i, s)| *s != cased[i])
        };
        // `order` played: what each op sent and, if each sent what it did
        // here, which failed and the readings (user code, run only then).
        let play = |order: &[usize], steps: &[&super::Step<O>]| -> Played {
            let (end, per) = play_serial(o.start, steps, o.end)?;
            let sent_by: Vec<Sent> = per.iter().map(|p| sent(p)).collect();
            let judged = other(order, &sent_by).is_none().then(|| {
                let failed_by = steps.iter().zip(&per).map(|(s, p)| failed(s, p)).collect();
                let flat: Vec<TxOutcome> = per.iter().flatten().cloned().collect();
                (failed_by, self.read(o.start, &end, &flat))
            });
            Ok((sent_by, judged))
        };
        let memo = o.memo.map(|(m, _)| m);
        #[cfg(test)]
        let memo = memo.filter(|_| !UNMEMOIZED.get());
        // `order` as the memo knows it, else played and kept. One known
        // only by what each op sent, which sent here what it did in the
        // case, was never judged: played again.
        let serially = |order: &[usize], steps: &[&super::Step<O>]| -> Played {
            let key = memo.zip(order_key(o, steps));
            let known = key.and_then(|(m, k)| m.get(&k));
            let unjudged = |p: &Played| matches!(p, Ok((s, None)) if other(order, s).is_none());
            if let Some(p) = known.filter(|p| !unjudged(p)) {
                return p;
            }
            let p = play(order, steps);
            if let Some((m, k)) = key {
                m.insert(k, p.clone());
            }
            p
        };
        // Every order of `ops`, run: its readings and which ops failed.
        let run = |ops: &[usize]| -> Result<Vec<Order>, Fail> {
            let mut out = vec![];
            for order in permutations(ops) {
                let steps: Vec<&super::Step<O>> = order.iter().map(|&i| o.ops[i].step).collect();
                let (sent_by, judged) = serially(&order, &steps).map_err(|why| {
                    Fail::not_judged(format!(
                        "serial order {} didn't run: {why}",
                        order_name(&order)
                    ))
                })?;
                if let Some(k) = other(&order, &sent_by) {
                    return Err(Fail::not_judged(format!(
                        "{} sent another call in serial order {} than in the case: its \
                         outcome isn't comparable",
                        names[order[k]],
                        order_name(&order)
                    )));
                }
                let Some((failed_by, readings)) = judged else {
                    unreachable!("an order each op sent as in the case is judged")
                };
                let mut failed_in = vec![false; n];
                order.iter().zip(failed_by).for_each(|(&i, f)| failed_in[i] = f);
                out.push((order, readings, failed_in));
            }
            Ok(out)
        };
        let every: Vec<usize> = (0..n).collect();
        let full = run(&every)?;
        // Refused only under interleaving: it fails here, never alone.
        for (i, r) in o.ops.iter().enumerate() {
            if failed(r.step, &r.outcomes) && full.iter().all(|(_, _, f)| !f[i]) {
                let why = format!(
                    "{} failed with {} in flight and succeeded in every serial order ({} of them)",
                    r.name,
                    names
                        .iter()
                        .enumerate()
                        .filter(|(j, _)| *j != i)
                        .map(|(_, s)| *s)
                        .collect::<Vec<_>>()
                        .join(", "),
                    full.len()
                );
                return Err(Fail::new(why).cause("refused only under interleaving"));
            }
        }
        // A failed op that moved nothing, and fails in some serial order,
        // may be left out.
        let droppable: Vec<usize> = (0..n)
            .filter(|&i| {
                let r = &o.ops[i];
                failed(r.step, &r.outcomes) && moved_nothing(&r.outcomes)
            })
            .filter(|&i| full.iter().any(|(_, _, f)| f[i]))
            .collect();
        // Each set of them: the empty one first.
        let mut sets: Vec<Vec<usize>> = vec![vec![]];
        for &i in &droppable {
            let with: Vec<Vec<usize>> = sets.iter().map(|s| [&s[..], &[i]].concat()).collect();
            sets.extend(with);
        }
        let mut orders = full;
        for gone in &sets[1..] {
            let kept: Vec<usize> = every.iter().copied().filter(|i| !gone.contains(i)).collect();
            orders.extend(run(&kept)?);
        }
        let Some((order, off)) = closest(&mine, &orders) else { return Ok(()) };
        let left =
            if order.len() < n { " (the failed ops that moved nothing left out)" } else { "" };
        Err(Fail::new(format!(
            "{} in flight together: no serial order of them gives this outcome; the closest, {}{left}, \
             differs at {}",
            names.join(", "),
            order_name(order),
            off.join("; ")
        )))
    }
}

/// The order of `orders` whose readings are closest to `mine`: fewest
/// holders off by more than their tolerance, then the least off, the
/// first of equals; with each holder off. `None` if one is off nowhere.
fn closest<'o>(mine: &Readings, orders: &'o [Order]) -> Option<(&'o [usize], Vec<String>)> {
    let mut best: Option<(usize, Delta, &[usize], Vec<String>)> = None;
    for (order, theirs, _) in orders {
        let keys: BTreeSet<&String> = mine.keys().chain(theirs.keys()).collect();
        let mut off = vec![];
        let mut most = Delta::ZERO;
        for k in keys {
            // A holder one side leaves out reads zero there, at the
            // other side's tolerance.
            let (a, b) = (mine.get(k), theirs.get(k));
            let tol = a.or(b).map_or(0, |r| r.1);
            let (a, b) = (a.map_or(Delta::ZERO, |r| r.0), b.map_or(Delta::ZERO, |r| r.0));
            let d = a - b;
            if d > Delta::gain(tol) || d < Delta::loss(tol) {
                most = most.max(d.max(-d));
                off.push(format!("{k} {a} here, {b} there"));
            }
        }
        if off.is_empty() {
            return None;
        }
        if best.as_ref().is_none_or(|b| (off.len(), most) < (b.0, b.1)) {
            best = Some((off.len(), most, order, off));
        }
    }
    best.map(|(_, _, order, off)| (order, off))
}

/// Every order of `items`, at most [`SERIAL_OPS`] of them: their count
/// grows as n!, so more is a bug in the caller, and panics before the
/// orders fill memory.
fn permutations(items: &[usize]) -> Vec<Vec<usize>> {
    assert!(items.len() <= SERIAL_OPS, "{} ops: {}! serial orders", items.len(), items.len());
    if items.len() <= 1 {
        return vec![items.to_vec()];
    }
    let mut out = vec![];
    for (k, &first) in items.iter().enumerate() {
        let rest: Vec<usize> =
            items.iter().enumerate().filter(|(j, _)| *j != k).map(|(_, &x)| x).collect();
        for mut tail in permutations(&rest) {
            tail.insert(0, first);
            out.push(tail);
        }
    }
    out
}

/// `steps` played on a clone of `start`, each settled before the next, as
/// `Gap::Settle` settles (a parked yield stays parked); after the last,
/// parked yields time out, as in `end`, the case's world after the
/// overlap, and the clock skips to `end`'s epoch, so the epoch rewards
/// match: the world left, and each step's transactions' outcomes.
/// `Err` if a step skips or panics, the chain doesn't settle, or the
/// order ran past `end`'s epoch.
fn play_serial<O: Op<W>, W: World>(
    start: &W,
    steps: &[&super::Step<O>],
    end: &W,
) -> Result<(W, Vec<Vec<TxOutcome>>), String> {
    #[cfg(test)]
    PLAYED.with(|n| n.set(n.get() + 1));
    let mut w = start.clone();
    // Each step's transactions, read from the world the whole order
    // leaves: a parked yield's callback runs in a later step's settle.
    let mut hashes = vec![];
    let epoch = end.as_ref().epoch_height();
    for (k, step) in steps.iter().enumerate() {
        let last = k + 1 == steps.len();
        let from = w.as_ref().txs().len();
        let hook = super::Hook::new(step.gas, step.inject.clone(), step.answers.clone());
        w.as_mut().explore = Some(Box::new(hook));
        let ran = guarded(|| match &step.act {
            super::Act::Op(op) => op.submit(&mut w),
            super::Act::Call(call) => {
                call.send(w.as_mut());
                Ok(())
            }
        });
        w.as_mut().explore = None;
        let name = format!("{:?}", step.act);
        match ran {
            Err(p) => return Err(format!("{name} panicked: {p}")),
            Ok(Err(skip)) => return Err(format!("{name} skipped: {skip}")),
            Ok(Ok(())) => {}
        }
        let settled = guarded(|| {
            let sim = w.as_mut();
            let max = sim.config.max_blocks;
            let done = |sim: &crate::Sim| sim.is_idle() && (!last || sim.yields().is_empty());
            for _ in 0..max {
                if done(sim) {
                    break;
                }
                sim.step();
            }
            if last && done(sim) && sim.epoch_height() < epoch {
                let length = sim.config.epoch_length;
                sim.advance_to((epoch - 1) * length + 1);
            }
            done(sim)
        });
        match settled {
            Ok(true) => {}
            Ok(false) => return Err(format!("not settled after {name}")),
            Err(p) => return Err(format!("{name}: {p}")),
        }
        hashes.push(w.as_ref().txs()[from..].to_vec());
    }
    let (here, there) = (epoch, w.as_ref().epoch_height());
    if there != here {
        return Err(format!("it ends in epoch {there}, the case in {here}"));
    }
    let sim = w.as_ref();
    let per = hashes.iter().map(|hs| hs.iter().map(|t| sim.outcome(t)).collect()).collect();
    Ok((w, per))
}

/// What a kept or backing failure allows for `proofs` 1-yocto proofs.
fn allowed(proofs: u128) -> String {
    match proofs {
        0 => String::new(),
        n => format!(" ({n} yocto allowed for {n} 1-yocto proof(s))"),
    }
}

/// `a * b` in 256 bits: (high, low).
fn wide(a: u128, b: u128) -> (u128, u128) {
    const M: u128 = u64::MAX as u128;
    let (a1, a0, b1, b0) = (a >> 64, a & M, b >> 64, b & M);
    let (ll, lh, hl) = (a0 * b0, a0 * b1, a1 * b0);
    let mid = (ll >> 64) + (lh & M) + (hl & M);
    ((a1 * b1) + (lh >> 64) + (hl >> 64) + (mid >> 64), (ll & M) | (mid << 64))
}

/// Each claim `c` lists in `w`, redeemed on a clone at its maturity.
fn liveness<O: Op<W>, W: World>(
    c: &ClaimDecl<O, W>,
    held: &Held<W>,
    near: &Option<AccountId>,
    ceiling: StepGas,
    w: &W,
    play: &Player<O, W>,
) -> Result<(), Fail> {
    let claims = (c.open)(w);
    if claims.is_empty() {
        return Err(Fail::idle(format!("no {} open", c.name)));
    }
    for claim in claims {
        let now = w.as_ref().epoch_height();
        let holder = &claim.holder;
        if let Some(m) = claim.matures.filter(|&m| m > now + LIVENESS_EPOCHS) {
            return Err(Fail::not_judged(format!(
                "{holder}'s claim matures at epoch {m}, past the {LIVENESS_EPOCHS} epochs liveness \
                 advances (now {now})"
            )));
        }
        let mut w = w.clone();
        while w.as_ref().epoch_height() < claim.matures.unwrap_or(0) {
            w.as_mut().step();
        }
        let mut redeem = (c.redeem)(&claim);
        redeem.steps.iter_mut().for_each(|s| s.gas = Some(s.gas.unwrap_or(ceiling)));
        let before = held(&w);
        let outcomes = play(&mut w, &redeem);
        if outcomes.is_empty() {
            return Err(Fail::not_judged(format!("{holder}'s redeem submitted nothing")));
        }
        let paid = match near {
            Some(id) => (-merge(&outcomes).near_net(id)).max(Delta::ZERO).magnitude(),
            None => before.saturating_sub(held(&w)),
        };
        let open = (c.open)(&w).iter().any(|x| x.holder == *holder);
        if paid < claim.amount || open {
            let left = if open { ", and it is still open" } else { "" };
            let why = format!("{holder}'s claim of {} redeemed {paid}{left}", claim.amount);
            return Err(Fail::new(why).key(holder.clone()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Balance, KeyType, SecretKey, Sim};

    /// The closest serial order: the fewest holders off, then the least
    /// off, a loss as far as a gain its size; the first of equals; none
    /// when one matches.
    #[test]
    fn the_closest_order_is_the_least_off_and_the_first_of_equals() {
        let read = |x: Delta, y: Delta| -> Readings {
            BTreeMap::from([("x".to_owned(), (x, 0)), ("y".to_owned(), (y, 0))])
        };
        let order = |k: usize, x, y| (vec![k], read(x, y), vec![]);
        let mine = read(Delta::ZERO, Delta::ZERO);
        let orders = [
            order(0, Delta::gain(1), Delta::gain(1)),
            order(1, Delta::gain(5), Delta::ZERO),
            order(2, Delta::loss(1), Delta::ZERO),
            order(3, Delta::loss(1), Delta::ZERO),
        ];
        let (best, off) = closest(&mine, &orders).unwrap();
        assert_eq!((best, off.len()), (&[2][..], 1), "{off:?}");
        assert!(off[0].starts_with("x "), "{off:?}");
        let orders = [orders[0].clone(), order(4, Delta::ZERO, Delta::ZERO)];
        assert!(closest(&mine, &orders).is_none());
    }

    /// A holder one side leaves out reads zero there, at the other side's
    /// tolerance: as if it read zero.
    #[test]
    fn a_holder_left_out_keeps_its_tolerance() {
        let read = |d: Delta| -> Readings { BTreeMap::from([("x".to_owned(), (d, 1))]) };
        let order = |r: Readings| [(vec![0], r, vec![])];
        assert!(closest(&Readings::new(), &order(read(Delta::gain(1)))).is_none());
        assert!(closest(&read(Delta::loss(1)), &order(Readings::new())).is_none());
        assert!(closest(&Readings::new(), &order(read(Delta::gain(2)))).is_some());
    }

    /// A NEAR asset's reading adds the epoch rewards its stake was paid
    /// to what moved in and out: a staked pool's, epochs on, reads its
    /// rewards as a gain.
    #[test]
    fn a_near_reading_adds_the_rewards() {
        const POOL: &str = "pool.near";
        let mut sim = Sim::new();
        sim.config.epoch_length = 10;
        sim.config.epoch_reward = Balance::from_near(1);
        sim.create_account("alice.near", Balance::from_near(1000));
        sim.create_account(POOL, Balance::from_near(2));
        let wat = include_str!("../../tests/res/staking_pool.wat");
        sim.deploy(POOL, wat::parse_str(wat).unwrap());
        let key = SecretKey::from_seed(KeyType::ED25519, POOL).public_key();
        sim.call(POOL, POOL, "new").args(borsh::to_vec(&key).unwrap()).transact().assert_success();
        let stake = sim.call("alice.near", POOL, "deposit_and_stake");
        stake.deposit(Balance::from_near(100)).transact().assert_success();
        let start = sim.clone();
        let epoch = sim.epoch_height();
        while sim.epoch_height() < epoch + 4 {
            sim.step();
        }
        let paid = sim.rewarded(POOL).as_yoctonear() - start.rewarded(POOL).as_yoctonear();
        assert!(paid > 0, "no reward paid");
        let id: AccountId = POOL.parse().unwrap();
        let of = id.clone();
        let held: Held<Sim> =
            Rc::new(move |w: &Sim| w.account(&of).map_or(0, |a| a.amount.as_yoctonear()));
        let serial =
            Serial { liabilities: vec![], assets: vec![("pool NEAR".into(), held, Some(id), 0)] };
        let readings = serial.read(&start, &sim, &[]);
        assert_eq!(readings["pool NEAR"].0, Delta::gain(paid));
    }
}
