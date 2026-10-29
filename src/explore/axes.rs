//! [`Axes`]: what the environment controls in a step, found from the op's
//! serde form and from what the witness ran. Every number is an axis (a
//! scan tries its edges, a curve moves it), every list's length and every
//! string's bytes are too; so are the caller's gas, how many times the
//! step's act or a call its receipts made is repeated before it, and how
//! many such calls land while one of its callbacks waits. An op is moved
//! along an axis by editing its JSON and deserializing it, so
//! [`Op::submit`](super::Op::submit) signs again whatever it signs.

use super::plan::{Act, Inject, Plan, Step, StepGas, Swept};
use super::{Call, Gap, Harness, Op, Run, World};
use crate::{Fault, ReceiptOutcome, Receipts};
use near_primitives::types::Gas;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fmt;

/// What moving an axis changes.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum AxisKind {
    /// A list's length. Longer lists end with copies of the last item,
    /// made fresh (distinct ids, accounts and names; numbers kept); lists
    /// of the same length beside it grow with it.
    Len,
    /// A string's bytes.
    Bytes,
    /// A number's value; a decimal string (a `u128` amount) included.
    /// One the witness leaves unset (`null`) is tried at a scan's edges
    /// only, as whichever of a number or a decimal string its type takes.
    Number,
    /// An enum's variant: its others, from the type's own error, each a
    /// unit variant or carrying data (the witness's, else a plain payload
    /// its errors fill in). A scan tries each; curves leave it alone.
    Choice,
    /// The gas the step's unpinned calls attach.
    CallerGas,
    /// Copies of the step's act, or of a call one of its receipts made
    /// resubmitted from a signer of its own, each fresh
    /// ([`Op::copy`](super::Op::copy)) and settled before the next, so
    /// each reads the world as the last left it.
    Repeat,
    /// Such copies sent two blocks apart while one of the step's callbacks
    /// is held: what lands between a call and its callback.
    HoldTail,
}

/// One axis of a witness's step: its name (a JSON pointer into the op,
/// `caller gas`, `repeat keeper.near::on_transfer`, ...), its kind, and
/// its value in the witness.
#[doc(hidden)]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Axis {
    /// `/Withdraw/min_gas`, `caller gas`, `repeat Deposit`, ...
    pub name: String,
    /// What moving it changes.
    pub kind: AxisKind,
    /// Its value in the witness: 0 for a repeat or a tail.
    pub at: u128,
    pub(crate) how: How,
}

/// How an axis moves a plan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum How {
    /// The act's JSON at `pointer`; a number written as a string if
    /// `string`.
    Arg { pointer: String, string: bool },
    /// A number the witness leaves unset (`null`) at `pointer`.
    Unset { pointer: String, string: bool },
    /// The enum at `pointer`, set to `values[value]`.
    Choice { pointer: String, values: Vec<Value> },
    /// The step's gas.
    Gas,
    /// Copies before the step.
    Repeat(CopySource),
    /// Copies sent while the receipt `held` (`receiver::method`) waits:
    /// the first `after` blocks past the step, when the witness ran it.
    Tail { held: String, after: u16, copy: CopySource },
}

/// What a repeat or a tail copies.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum CopySource {
    /// The act of this step of the plan.
    Step(usize),
    /// This call, as a receipt received it, from [`SIGNER`]; and the op of
    /// the witness's step that made it, if an op did (its JSON), whose
    /// copies make the call's copies ([`Copied`](super::Copied)).
    Call(Call, Option<Value>),
    /// This call, as the step's entry received it from its signer, each
    /// copy from a fresh one: callers new to the contract; and the op, as
    /// for `Call`.
    Fresh(Call, Option<Value>),
}

/// `/Withdraw/min_gas (number)`.
impl fmt::Display for Axis {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self.kind {
            AxisKind::Len => "length",
            AxisKind::Bytes => "bytes",
            AxisKind::Number if matches!(self.how, How::Unset { .. }) => "number, unset",
            AxisKind::Number => "number",
            AxisKind::Choice => "choice",
            AxisKind::CallerGas | AxisKind::Repeat | AxisKind::HoldTail => {
                return f.write_str(&self.name);
            }
        };
        write!(f, "{} ({kind})", self.name)
    }
}

impl Axis {
    /// Whether curves climb it: not a choice, nor a number the witness
    /// left unset (a scan tries those at their edges).
    pub(crate) fn climbs(&self) -> bool {
        !matches!(self.how, How::Choice { .. } | How::Unset { .. })
    }
}

/// The signer a resubmitted call comes from, made if the world lacks it:
/// a fixture that needs it registered (a token's `storage_deposit`)
/// creates and registers it, and that one is used as is.
pub(crate) const SIGNER: &str = "axis.near";

impl<O: Op<W>, W: World> Harness<O, W> {
    /// Every axis of each step of `plan`, as a run of it shows them:
    /// `(step, axis)`. Each moves with [`moved`](Self::moved).
    #[doc(hidden)]
    pub fn axes(&self, plan: &Plan<O>) -> Vec<(usize, Axis)> {
        let run = self.run_written(plan);
        (0..plan.steps.len())
            .flat_map(|i| axes_of(self, &run, i).0.into_iter().map(move |a| (i, a)))
            .collect()
    }

    /// Whether `account`'s contract is audited ([`audits`](Self::audits)).
    pub(crate) fn audited(&self, account: &str) -> bool {
        self.audits.as_ref().is_none_or(|a| a.iter().any(|x| x == account))
    }

    /// `plan` with step `step` moved to `value` along `axis`, or `None`
    /// if its act doesn't rebuild there. A repeat or a tail adds steps:
    /// fresh copies of the act, or of the call, from `axis.near`.
    #[doc(hidden)]
    pub fn moved(&self, plan: &Plan<O>, step: usize, axis: &Axis, value: u128) -> Option<Plan<O>> {
        moved::<O, W>(plan, step, axis, value)
    }
}

/// Which of an op's arguments are axes ([`Op::axes`](super::Op::axes)).
/// [`auto`](Self::auto), the default, takes every one; narrow a large op
/// with [`only`](Self::only); name a documented gas floor that isn't a
/// constant of the wasm with [`floor`](Self::floor).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Axes {
    only: Option<Vec<String>>,
    floors: Vec<(String, Gas)>,
}

/// One leaf of an op's JSON that is an axis, as the witness has it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Leaf {
    /// A number, or a decimal string (`string`).
    Number { value: u128, string: bool },
    /// A string of this many bytes.
    Bytes(usize),
    /// A list of this many items.
    Len(usize),
    /// `null`.
    Null,
    /// An object of one key, perhaps an enum's variant carrying data.
    Tagged,
}

impl Axes {
    /// Every number, list and string of the op.
    pub fn auto() -> Self {
        Axes::default()
    }

    /// Only the arguments under these JSON pointers (`/Deposit/amount`,
    /// or a prefix such as `/Deposit`), and only the repeats and tails
    /// whose names start with an entry that isn't a pointer (`repeat`,
    /// `repeat fresh callers`, `repeat Deposit`): none unless named. The
    /// caller's gas stays an axis.
    pub fn only<'a>(pointers: impl IntoIterator<Item = &'a str>) -> Self {
        Axes { only: Some(pointers.into_iter().map(str::to_owned).collect()), floors: vec![] }
    }

    /// The number at `pointer` is a gas value compared against `floor`, a
    /// documented floor that isn't a constant of the wasm: a scan tries
    /// the floor and 1 gas either side, and the wasm's gas constants from
    /// the floor up.
    pub fn floor(mut self, pointer: &str, floor: Gas) -> Self {
        self.floors.push((pointer.to_owned(), floor));
        self
    }

    /// The floor named for `pointer`, if any.
    pub(crate) fn floor_of(&self, pointer: &str) -> Option<Gas> {
        self.floors.iter().find(|(p, _)| p == pointer).map(|(_, g)| *g)
    }

    /// The axes of `json`, an op's serde form, with their witness values.
    pub(crate) fn leaves(&self, json: &Value) -> Vec<(String, Leaf)> {
        let mut out = vec![];
        walk(json, &mut String::new(), &mut out);
        let wanted = |p: &str| {
            self.only.as_ref().is_none_or(|only| {
                only.iter().any(|o| {
                    p == o || p.strip_prefix(o.as_str()).is_some_and(|r| r.starts_with('/'))
                })
            })
        };
        out.retain(|(name, _)| wanted(name));
        out
    }

    /// Whether the repeat or tail named `name` is an axis.
    fn copies(&self, name: &str) -> bool {
        let named = |o: &String| !o.starts_with('/') && name.starts_with(o.as_str());
        self.only.as_ref().is_none_or(|only| only.iter().any(named))
    }
}

fn walk(v: &Value, path: &mut String, out: &mut Vec<(String, Leaf)>) {
    let mut push = |leaf| out.push((path.clone(), leaf));
    match v {
        Value::Number(n) => {
            if let Ok(value) = n.to_string().parse::<u128>() {
                push(Leaf::Number { value, string: false });
            }
        }
        Value::String(s) => match s.parse::<u128>() {
            Ok(value) if s.bytes().all(|b| b.is_ascii_digit()) => {
                push(Leaf::Number { value, string: true });
            }
            _ => push(Leaf::Bytes(s.len())),
        },
        Value::Array(items) => {
            push(Leaf::Len(items.len()));
            for (i, item) in items.iter().enumerate() {
                let len = path.len();
                path.push_str(&format!("/{i}"));
                walk(item, path, out);
                path.truncate(len);
            }
        }
        Value::Object(map) => {
            if map.len() == 1 && !path.is_empty() {
                push(Leaf::Tagged);
            }
            for (k, item) in map {
                let len = path.len();
                path.push('/');
                path.push_str(&pointer_key(k));
                walk(item, path, out);
                path.truncate(len);
            }
        }
        Value::Null => push(Leaf::Null),
        Value::Bool(_) => {}
    }
}

/// The act's serde form and its declared axes: an op's, or a call's
/// args.
pub(crate) fn act_json<O: Op<W>, W: World>(act: &Act<O>) -> Option<(Value, Axes)> {
    let axes = act.op().map_or_else(|| Axes::only(["/args"]), |op| op.axes());
    Some((act.json()?, axes))
}

/// The largest value the act's type takes at `pointer`.
pub(crate) fn act_max<O: Op<W>, W: World>(act: &Act<O>, json: &Value, axis: &Axis) -> Option<u128> {
    let (How::Arg { pointer, string } | How::Unset { pointer, string }) = &axis.how else {
        return None;
    };
    match act {
        Act::Op(_) => type_max::<O>(json, pointer, *string),
        Act::Call(_) => type_max::<Call>(json, pointer, *string),
    }
}

/// The axes of the act's arguments. A list's items move together: one
/// axis per field of its items (`/ids/*`), at the largest item's value.
pub(crate) fn arg_axes<O: Op<W>, W: World>(act: &Act<O>) -> Vec<Axis> {
    args::<O, W>(act).0
}

/// [`arg_axes`], and the strings that aren't axes: changed, they don't
/// deserialize, and no other value is found for them.
pub(crate) fn args<O: Op<W>, W: World>(act: &Act<O>) -> (Vec<Axis>, Vec<String>) {
    let Some((json, axes)) = act_json::<O, W>(act) else { return (vec![], vec![]) };
    // A call's JSON args are any JSON value, so an edit inside them always
    // rebuilds: no need to deserialize the whole call per leaf.
    let any = |at: &str| {
        matches!(act, Act::Call(_)) && (at == "/args/json" || at.starts_with("/args/json/"))
    };
    let builds = |at: &str, j: Option<Value>| j.is_some_and(|j| any(at) || act.rebuild(j).is_ok());
    let (mut out, mut fixed): (Vec<Axis>, Vec<String>) = (vec![], vec![]);
    for (at, leaf) in axes.leaves(&json) {
        let parts = at.split('/').map(|p| if p.parse::<usize>().is_ok() { "*" } else { p });
        let name = parts.collect::<Vec<_>>().join("/");
        let arg = |string| How::Arg { pointer: name.clone(), string };
        let (kind, at, how) = match leaf {
            Leaf::Number { value, string } => (AxisKind::Number, value, arg(string)),
            // A tuple's length is fixed: one item more, on it alone,
            // doesn't rebuild.
            Leaf::Len(n) if n > 0 && !builds(&at, longer(&json, &at)) => {
                fixed.push(name);
                continue;
            }
            Leaf::Len(n) => (AxisKind::Len, n as u128, arg(false)),
            Leaf::Bytes(n) if builds(&at, with_bytes(&json, &at, n + 1)) => {
                (AxisKind::Bytes, n as u128, arg(false))
            }
            Leaf::Bytes(_) | Leaf::Tagged => {
                let values = choices::<O, W>(act, &json, &at);
                let tag = |v: &Value| v.as_object().and_then(|m| m.keys().next().cloned());
                let current = json.pointer(&at);
                let same = |v: &Value| tag(v).is_some_and(|t| current.and_then(tag) == Some(t));
                let k = values.iter().position(|v| Some(v) == current || same(v));
                match k {
                    Some(k) if values.len() > 1 => {
                        (AxisKind::Choice, k as u128, How::Choice { pointer: name.clone(), values })
                    }
                    _ if leaf == Leaf::Tagged => continue,
                    _ => {
                        fixed.push(name);
                        continue;
                    }
                }
            }
            Leaf::Null => {
                let text = |t: &str| {
                    let mut j = json.clone();
                    *j.pointer_mut(&at)? = Value::String(t.into());
                    Some(j)
                };
                let string = if builds(&at, Some(with_number(&json, &at, 0, false))) {
                    false
                } else if builds(&at, text("0")) && !builds(&at, text("x")) {
                    true
                } else {
                    continue;
                };
                (AxisKind::Number, 0, How::Unset { pointer: name.clone(), string })
            }
        };
        // The root pointer is the whole op, replaced (an enum's variant).
        let name = if name.is_empty() { "the op".to_owned() } else { name };
        match out.iter_mut().find(|a| a.name == name && a.kind == kind) {
            Some(a) => a.at = a.at.max(at),
            None => out.push(Axis { how, name, kind, at }),
        }
    }
    fixed.dedup();
    (out, fixed)
}

/// Each step's strings that aren't axes ([`args`]), named by step.
pub(crate) fn fixed<O: Op<W>, W: World>(plan: &Plan<O>) -> Vec<String> {
    let steps = plan.steps.iter().enumerate();
    steps
        .flat_map(|(i, s)| {
            args::<O, W>(&s.act).1.into_iter().map(move |n| format!("{n} (step {i})"))
        })
        .collect()
}

/// The values the enum at `pointer` takes: each variant the act's type
/// names when it refuses another, that deserializes there as a unit
/// variant, or carrying the witness's data (its own variant) or a plain
/// payload ([`plain`]).
fn choices<O: Op<W>, W: World>(act: &Act<O>, json: &Value, pointer: &str) -> Vec<Value> {
    let set = |v: Value| {
        let mut j = json.clone();
        *j.pointer_mut(pointer)? = v;
        Some(j)
    };
    let error = |v: Value| match set(v) {
        Some(j) => act.rebuild(j).err().map(|e| e.to_string()),
        None => Some(String::new()),
    };
    let Some(refusal) = error("\u{1}".into()) else { return vec![] };
    let mut out = vec![];
    for v in serde_says::variants(&refusal) {
        let own = json.pointer(pointer).and_then(|j| Some(j.as_object()?.get(v)?.clone()));
        let data = |d: Value| Value::Object([(v.to_owned(), d)].into_iter().collect());
        let payload = own.or_else(|| plain(&|d| error(data(d.clone())).map_or(Ok(()), Err), &[]));
        let built = |c: &Value| set(c.clone()).is_some_and(|j| act.rebuild(j).is_ok());
        out.extend([Value::String(v.into())].into_iter().chain(payload.map(data)).find(built));
    }
    out
}

/// A value `ok` takes: `first` or a plain one, [`fill`]ed.
pub(crate) fn plain(ok: &dyn Fn(&Value) -> Result<(), String>, first: &[Value]) -> Option<Value> {
    first.iter().cloned().chain(plains()).find_map(|v| fill(ok, first, v))
}

/// Plain values, in the order [`fill`] tries them.
fn plains() -> [Value; 7] {
    [Value::Null, json!(0), json!(false), json!(""), json!("0"), json!([]), json!({})]
}

/// What an op type takes, read from serde_json's errors: an op is any
/// serde type, and serde publishes no schema of it. The unit test pins
/// the error text these read.
mod serde_says {
    /// The variants an `unknown variant` error lists after `expected`.
    pub(super) fn variants(error: &str) -> Vec<&str> {
        let listed = error.split_once("expected ").map_or("", |(_, l)| l);
        listed.split('`').skip(1).step_by(2).collect()
    }

    /// The field a `missing field` error names.
    pub(super) fn missing_field(error: &str) -> Option<&str> {
        error.split("missing field `").nth(1)?.split('`').next()
    }
}

/// An object key as a JSON pointer part (RFC 6901): `~` and `/` escaped.
fn pointer_key(k: &str) -> String {
    k.replace('~', "~0").replace('/', "~1")
}

/// `v` with each field `ok`'s error names missing added, to the first
/// object of `v` where one of `extra` or [`plains`] gets past it, until
/// `ok` takes it; `None` if a field gets past nowhere.
pub(crate) fn fill(
    ok: &dyn Fn(&Value) -> Result<(), String>,
    extra: &[Value],
    mut v: Value,
) -> Option<Value> {
    fn objects(v: &Value, at: String, out: &mut Vec<String>) {
        match v {
            Value::Object(m) => {
                out.push(at.clone());
                m.iter().for_each(|(k, v)| objects(v, format!("{at}/{}", pointer_key(k)), out));
            }
            Value::Array(a) => {
                a.iter().enumerate().for_each(|(i, v)| objects(v, format!("{at}/{i}"), out))
            }
            _ => {}
        }
    }
    let missing = |v: &Value| Some(serde_says::missing_field(&ok(v).err()?)?.to_owned());
    let values: Vec<Value> = extra.iter().cloned().chain(plains()).collect();
    while let Some(f) = missing(&v) {
        let mut at = vec![];
        objects(&v, String::new(), &mut at);
        let past = |c: &Value| ok(c).is_ok() || missing(c).is_some_and(|g| g != f);
        v = at.iter().flat_map(|p| values.iter().map(move |z| (p, z))).find_map(|(p, z)| {
            let mut c = v.clone();
            let obj = c.pointer_mut(p)?.as_object_mut()?;
            obj.insert(f.clone(), z.clone()).is_none().then_some(())?;
            past(&c).then_some(c)
        })?;
    }
    ok(&v).is_ok().then_some(v)
}

/// The pointers `pointer` names in `json`, each `*` any index of a list.
fn expand(json: &Value, pointer: &str) -> Vec<String> {
    let Some((head, rest)) = pointer.split_once("/*") else { return vec![pointer.to_owned()] };
    let n = json.pointer(head).and_then(Value::as_array).map_or(0, Vec::len);
    (0..n).flat_map(|i| expand(json, &format!("{head}/{i}{rest}"))).collect()
}

/// Every axis of `run`'s step `i`: its arguments; the caller's gas if a
/// call of it is unpinned; repeats of its act and of each call one of its
/// receipts made from another account into an audited contract; and, for
/// each callback of a contract to itself, a tail of each call one of its
/// receipts made to that contract. Then the names of the repeats and tails the op's
/// [`Axes::only`] left out.
pub(crate) fn axes_of<O: Op<W>, W: World>(
    h: &Harness<O, W>,
    run: &Run<O>,
    i: usize,
) -> (Vec<Axis>, Vec<String>) {
    let Some(step) = run.plan.steps.get(i) else { return (vec![], vec![]) };
    let mut out = arg_axes::<O, W>(&step.act);
    let Some(record) = run.ops.get(i).filter(|r| r.skipped.is_none()) else {
        return (out, vec![]);
    };
    if record.unpinned > 0 {
        let at = if let Some(StepGas::Exactly(g)) = step.gas { g } else { Gas::from_teragas(300) };
        out.push(Axis {
            name: "caller gas".into(),
            kind: AxisKind::CallerGas,
            at: at.as_gas().into(),
            how: How::Gas,
        });
    }
    let name = |j: usize| format!("{} (step {j})", run.plan.steps[j].act.name());
    let receipts = run.step_txs(i).flat_map(|o| {
        let sent = o.transaction.as_ref().map_or(0, |t| t.block_height);
        o.receipts.iter().skip(1).map(move |r| (sent, r))
    });
    let call = |r: &ReceiptOutcome| {
        Call::new(SIGNER, &r.receiver_id, &r.method_names[0])
            .args(r.args[0].clone())
            .deposit(r.deposit)
    };
    let of = step.act.op().and_then(|op| serde_json::to_value(op).ok());
    let mut calls: Vec<(String, CopySource)> = vec![];
    for r in run.step_txs(i).filter_map(|o| o.receipts.first()) {
        if r.method_names.len() == 1 && r.predecessor_id != r.receiver_id && r.failure().is_none() {
            let site = format!("fresh callers of {}::{}", r.receiver_id, r.method_names[0]);
            let mut entry = call(r);
            entry.signer = r.predecessor_id.clone();
            calls.push((site, CopySource::Fresh(entry, of.clone())));
        }
    }
    let mut held: Vec<(String, u16)> = vec![];
    // A refund is a transfer: it names no method.
    for (sent, r) in receipts.filter(|(_, r)| r.method_names.len() == 1) {
        let site = format!("{}::{}", r.receiver_id, r.method_names[0]);
        if r.predecessor_id == r.receiver_id {
            let after = r.block_height.saturating_sub(sent).clamp(1, u16::MAX.into());
            held.push((site, after as u16));
        } else if !calls.iter().any(|(s, _)| *s == site) {
            calls.push((site, CopySource::Call(call(r), of.clone())));
        }
    }
    held.dedup_by(|a, b| a.0 == b.0);
    let repeat = |name: String, copy: CopySource| Axis {
        name: format!("repeat {name}"),
        kind: AxisKind::Repeat,
        at: 0,
        how: How::Repeat(copy),
    };
    let mut copies = vec![repeat(name(i), CopySource::Step(i))];
    // A raw call is re-sent alone only into an audited contract (direct
    // witnesses' rule): one into a counterparty's (a token's) fails there
    // for what the fixture never set up. Its tails stay: they test the
    // held contract.
    let alone = |c: &CopySource| match c {
        CopySource::Call(call, _) => h.audited(call.receiver.as_str()),
        _ => true,
    };
    let repeats = calls.iter().filter(|(_, c)| alone(c));
    copies.extend(repeats.map(|(site, c)| repeat(site.clone(), c.clone())));
    calls.retain(|c| matches!(c.1, CopySource::Call(..)));
    // A tail copies the observed calls, this step and the later ones, to
    // the contract whose callback is held.
    let entry =
        |j: usize| run.step_txs(j).find_map(|o| Some(o.receipts.first()?.receiver_id.to_string()));
    let steps =
        (i..run.plan.steps.len()).filter_map(|j| Some((entry(j)?, (name(j), CopySource::Step(j)))));
    let steps: Vec<(String, (String, CopySource))> = steps.collect();
    for (h, after) in held {
        let on = h.split("::").next().unwrap_or_default();
        let observed = calls.iter().filter(|(s, _)| s.split("::").next() == Some(on));
        let own = steps.iter().filter(|(r, _)| r == on).map(|(_, c)| c);
        for (site, copy) in observed.chain(own) {
            let how = How::Tail { held: h.clone(), after, copy: copy.clone() };
            let name = format!("{site} while {h} is held");
            copies.push(Axis { name, kind: AxisKind::HoldTail, at: 0, how });
        }
    }
    let axes = step.act.op().map_or_else(Axes::auto, |op| op.axes());
    let (kept, dropped): (Vec<Axis>, Vec<Axis>) =
        copies.into_iter().partition(|a| axes.copies(&a.name));
    out.extend(kept);
    (out, dropped.into_iter().map(|a| a.name).collect())
}

/// `plans`' runs, or only the first if a check fails on the fixture,
/// which fails every case alike.
pub(crate) fn direct<O: Op<W>, W: World>(
    h: &Harness<O, W>,
    plans: &[Plan<O>],
) -> Vec<(Run<O>, Option<String>)> {
    let mut runs: Vec<(Run<O>, Option<String>)> = vec![];
    for p in plans {
        let run = h.run_case(p);
        if run.on_fixture {
            return vec![(run, None)];
        }
        runs.push((run, None));
    }
    runs
}

/// The outsider sweep: each call `runs`' steps made into an audited
/// contract (an entry call, another contract's call, a call to itself or
/// a callback), sent from [`SIGNER`] once per method after the earlier
/// steps of the witness that made it, so what those steps left is there
/// (not what the receipt's own step did before it), unless a witness
/// already sends it from there; each with the label of what it shows.
/// Its args and deposit are those of the first such receipt that
/// succeeded, else of the first; a function call batched with other
/// actions goes alone, with no deposit if a transfer was among them. A method every
/// witness saw called only by its own contract, and never as a
/// transaction's first receipt, is judged by
/// [`SELF_ONLY`](super::builtin::SELF_ONLY) there. Also every site sent
/// from [`SIGNER`], sorted.
pub(crate) fn sweep_plans<O: Op<W>, W: World>(
    h: &Harness<O, W>,
    runs: &[(Run<O>, Option<String>)],
) -> (Vec<(Plan<O>, String)>, Vec<String>) {
    // Per site, the plan (and whether its receipt succeeded), and whether
    // another account than its own contract made it.
    let mut calls: Vec<(String, Plan<O>, bool, bool)> = vec![];
    for (run, _) in runs {
        for i in 0..run.plan.steps.len() {
            for o in run.step_txs(i) {
                let root = o.first_call().map(|r| r.receipt_id);
                for r in o.receipts.iter().filter(|r| r.refund.is_none()) {
                    let [method] = &r.method_names[..] else { continue };
                    if !h.audited(r.receiver_id.as_str()) {
                        continue;
                    }
                    let site = format!("{}::{method}", r.receiver_id);
                    let ok = r.failure().is_none();
                    let plan = || {
                        // A batch's deposit is its actions': a transfer's too.
                        let transfer = r.actions.iter().any(|a| a == "Transfer");
                        let deposit = if transfer { crate::Balance::ZERO } else { r.deposit };
                        let call = Call::new(SIGNER, &r.receiver_id, method)
                            .args(r.args[0].clone())
                            .deposit(deposit);
                        let mut p = run.plan.clone();
                        p.steps.truncate(i);
                        p.call(call)
                    };
                    let other = r.predecessor_id != r.receiver_id || root == Some(r.receipt_id);
                    match calls.iter_mut().find(|c| c.0 == site) {
                        Some(c) => {
                            c.3 |= other;
                            if ok && !c.2 {
                                (c.1, c.2) = (plan(), true);
                            }
                        }
                        None => calls.push((site.clone(), plan(), ok, other)),
                    }
                }
            }
        }
    }
    let mut sent: Vec<String> =
        runs.iter().filter_map(|(r, _)| super::outsider::sent_site(h, &r.plan)).collect();
    let mut out = vec![];
    for (site, mut plan, _, other) in calls {
        if sent.contains(&site) {
            continue;
        }
        plan.swept = Some(Swept { site: site.clone(), self_only: !other });
        out.push((plan, format!("{site} sent directly from {SIGNER}")));
        sent.push(site);
    }
    sent.sort();
    sent.dedup();
    (out, sent)
}

/// Curves' direct witnesses: each call into an audited contract that a
/// step received from another account (a repeat axis of it), from
/// [`SIGNER`], once per method, unless a witness is already one call to
/// that method.
pub(crate) fn other_contracts<O: Op<W>, W: World>(
    h: &Harness<O, W>,
    runs: &[(Run<O>, Option<String>)],
) -> Vec<(Plan<O>, String)> {
    let same = |p: &Plan<O>, c: &Call| {
        let one = |d: &Call| d.receiver == c.receiver && d.method == c.method;
        matches!(&p.steps[..], [Step { act: Act::Call(d), .. }] if one(d))
    };
    let mut out: Vec<(Plan<O>, String)> = vec![];
    for (run, _) in runs {
        for a in (0..run.plan.steps.len()).flat_map(|i| axes_of(h, run, i).0) {
            let How::Repeat(CopySource::Call(c, _)) = a.how else { continue };
            let known = |p: &Plan<O>| same(p, &c);
            if h.audited(c.receiver.as_str())
                && !out.iter().any(|(p, _)| known(p))
                && !runs.iter().any(|r| known(&r.0.plan))
            {
                let at = format!("{}::{} sent directly from {}", c.receiver, c.method, c.signer);
                out.push((Plan::new().call(c), at));
            }
        }
    }
    out
}

/// `plan` with step `i` moved to `value` along `axis`, or `None` if the
/// act doesn't rebuild there (a list with nothing to copy, a value its
/// type refuses).
pub(crate) fn moved<O: Op<W>, W: World>(
    plan: &Plan<O>,
    i: usize,
    axis: &Axis,
    value: u128,
) -> Option<Plan<O>> {
    moved_with::<O, W>(plan, i, axis, value, true)
}

/// [`moved`], a longer list's new items fresh or, if not `fresh`, plain
/// copies of its last.
pub(crate) fn moved_with<O: Op<W>, W: World>(
    plan: &Plan<O>,
    i: usize,
    axis: &Axis,
    value: u128,
    fresh_items: bool,
) -> Option<Plan<O>> {
    let mut p = plan.clone();
    let step = p.steps.get_mut(i)?;
    let n = usize::try_from(value).ok();
    // A raw call copied from the witness's op refers to the op, held once
    // in the plan's sources: copy `k` of it makes the call (`Copied`).
    let op_json = match &axis.how {
        How::Repeat(CopySource::Call(_, of) | CopySource::Fresh(_, of))
        | How::Tail { copy: CopySource::Call(_, of) | CopySource::Fresh(_, of), .. } => of.clone(),
        _ => None,
    };
    let source = op_json.as_ref().map(|j| plan.sources.iter().position(|s| s == j));
    let source = source.map(|at| at.unwrap_or(plan.sources.len()));
    let copied = |k: usize, signer: Option<&Call>| {
        let (items, fresh, signer) =
            (fresh_items, signer.is_some(), signer.map(|c| c.signer.clone()));
        source.map(|source| Box::new(super::Copied { source, k, items, fresh, signer }))
    };
    let copies = |copy: &CopySource, n: usize, then: Gap| {
        let (act, gas) = match copy {
            CopySource::Step(j) => (plan.steps.get(*j)?.act.clone(), plan.steps[*j].gas),
            CopySource::Call(c, _) | CopySource::Fresh(c, _) => (Act::Call(c.clone()), None),
        };
        let (json, _) = act_json::<O, W>(&act)?;
        (0..n)
            .map(|k| {
                // Fresh where the type takes it, else a plain copy.
                let new = Some(fresh(&json, k, false)).filter(|_| fresh_items);
                let mut act = (new.and_then(|j| act.rebuild(j).ok()))
                    .or_else(|| act.rebuild(json.clone()).ok())?;
                match (copy, &mut act) {
                    (CopySource::Fresh(original, _), Act::Call(c)) => {
                        c.signer = fresh_text(c.signer.as_str(), k).parse().ok()?;
                        c.copies = copied(k, Some(original));
                    }
                    (CopySource::Call(..), Act::Call(c)) => c.copies = copied(k, None),
                    (_, Act::Op(op)) => *op = op.copy(k),
                    _ => {}
                }
                Some(Step { act, then, gas, inject: vec![], answers: vec![] })
            })
            .collect::<Option<Vec<Step<O>>>>()
    };
    match &axis.how {
        How::Arg { pointer, string } | How::Unset { pointer, string } => {
            let (json, _) = act_json::<O, W>(&step.act)?;
            let edit = |fresh: bool, beside: bool| {
                let mut json = json.clone();
                for at in expand(&json.clone(), pointer) {
                    json = match axis.kind {
                        AxisKind::Number => with_number(&json, &at, value, *string),
                        AxisKind::Len => with_len(&json, &at, n?, (fresh, beside))?,
                        _ => with_bytes(&json, &at, n?)?,
                    };
                }
                step.act.rebuild(json).ok()
            };
            // A list grows fresh where its items' type takes it, the lists
            // beside it too where theirs do, else plain.
            let (f, plain) = (fresh_items, || edit(false, false).filter(|_| fresh_items));
            step.act = edit(f, f).or_else(|| edit(f, false)).or_else(plain)?;
            // A stub's answer as long as the list grows with it.
            let list = |p: &String| json.pointer(p).and_then(Value::as_array).map(Vec::len);
            let was = expand(&json, pointer).first().and_then(list).filter(|_| n.is_some());
            for a in step.answers.iter_mut().filter(|_| axis.kind == AxisKind::Len) {
                if let (Value::Array(xs), Some(was)) = (&mut a.json, was)
                    && let Some(last) = xs.last().cloned().filter(|_| xs.len() == was)
                {
                    xs.resize(n?, last);
                }
            }
        }
        How::Choice { pointer, values } => {
            let v = values.get(n?)?.clone();
            let (mut json, _) = act_json::<O, W>(&step.act)?;
            for at in expand(&json.clone(), pointer) {
                *json.pointer_mut(&at)? = v.clone();
            }
            step.act = step.act.rebuild(json).ok()?;
        }
        How::Gas => step.gas = Some(StepGas::Exactly(Gas::from_gas(u64::try_from(value).ok()?))),
        How::Repeat(copy) => {
            p.callers = matches!(copy, CopySource::Fresh(..));
            p.steps.splice(i..i, copies(copy, n?, Gap::Settle)?);
        }
        How::Tail { held, after, copy } => {
            let n = n?;
            // Two blocks apart: each copy's entry has run when the next
            // reads the world.
            let mut steps = copies(copy, n, Gap::Blocks(2))?;
            if let Some(last) = steps.last_mut() {
                let (receiver, method) = held.split_once("::")?;
                let at = Receipts::call(receiver, method);
                step.inject.push(Inject { tx: 0, at, fault: Fault::Hold(2 * n as u64 + 2) });
                last.then = std::mem::replace(&mut step.then, Gap::Blocks(*after));
            }
            p.steps.splice(i + 1..i + 1, steps);
        }
    }
    let cited = |s: &Step<O>| matches!(&s.act, Act::Call(c) if c.copies.is_some());
    if let (Some(json), Some(at)) = (op_json, source)
        && at == p.sources.len()
        && p.steps.iter().any(cited)
    {
        p.sources.push(json);
    }
    Some(p)
}

/// Copy `k` of the op `json` holds, as a repeat or a tail copies an op's
/// step: its lists' items fresh first if `items`, then [`Op::copy`].
pub(crate) fn op_copy<O: Op<W>, W: World>(json: &Value, k: usize, items: bool) -> Option<O> {
    let op: O = rebuild(json.clone())?;
    let fresh = Some(fresh(json, k, false)).filter(|_| items).and_then(rebuild::<O>);
    Some(fresh.unwrap_or(op).copy(k))
}

/// `v` with each string `f` replaces (given whether it is, or is inside,
/// a list item) replaced.
fn map_strings(v: &Value, in_list: bool, f: &mut dyn FnMut(&str, bool) -> Option<String>) -> Value {
    match v {
        Value::String(s) => f(s, in_list).map_or_else(|| v.clone(), Value::String),
        Value::Array(items) => {
            Value::Array(items.iter().map(|x| map_strings(x, true, f)).collect())
        }
        Value::Object(map) => Value::Object(
            map.iter().map(|(key, x)| (key.clone(), map_strings(x, in_list, f))).collect(),
        ),
        _ => v.clone(),
    }
}

/// `v` made fresh for copy `k`: each string that is, or is inside, a
/// list item and isn't a number ([`fresh_text`]); numbers keep the
/// witness's value.
fn fresh(v: &Value, k: usize, in_list: bool) -> Value {
    let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    map_strings(v, in_list, &mut |s, in_list| (in_list && !digits(s)).then(|| fresh_text(s, k)))
}

/// `v` with each string naming a caller (`is_caller`: an account id with
/// no code, in the world) made fresh for copy `k`, as a fresh callers
/// axis copies an op that names its signer; the fresh ids.
pub(crate) fn fresh_callers(
    v: &Value,
    k: usize,
    is_caller: &dyn Fn(&near_primitives::types::AccountId) -> bool,
    made: &mut Vec<near_primitives::types::AccountId>,
) -> Value {
    map_strings(v, false, &mut |s, _| {
        s.parse().ok().filter(|id| is_caller(id))?;
        let new = fresh_text(s, k);
        made.extend(new.parse().ok());
        Some(new)
    })
}

/// `s` made fresh for copy `k`, still valid as what it is: an implicit
/// account id (hex) moves its last 8 hex digits, a named one's first
/// label ends in `-k`, an id after a `prefix:` keeps the prefix; other
/// text ends in `-k`, replacing its tail if it is 8 bytes or more (so a
/// long string keeps its length).
fn fresh_text(s: &str, k: usize) -> String {
    let id = |t: &str| t.parse::<near_primitives::types::AccountId>().is_ok();
    if let Some((head, tail)) = s.rsplit_once(':').filter(|(_, t)| id(t)) {
        return format!("{head}:{}", fresh_text(tail, k));
    }
    let tag = format!("-{k}");
    if id(s) {
        let body = s.strip_prefix("0x").or_else(|| s.strip_prefix("0s")).unwrap_or(s);
        if body.len() >= 40 && body.bytes().all(|b| b.is_ascii_hexdigit()) {
            let cut = s.len() - 8;
            let low = u32::from_str_radix(&s[cut..], 16).unwrap_or(0);
            return format!("{}{:08x}", &s[..cut], low.wrapping_add(k as u32 + 1));
        }
        let (first, rest) =
            s.split_once('.').map_or((s, String::new()), |(f, r)| (f, format!(".{r}")));
        let room = 64usize.saturating_sub(rest.len() + tag.len()).min(first.len());
        let named = format!("{}{tag}{rest}", first[..room].trim_end_matches(['-', '_']));
        if id(&named) {
            return named;
        }
    }
    let keep =
        (s.len() >= 8).then(|| (0..=s.len() - tag.len()).rev().find(|&c| s.is_char_boundary(c)));
    let keep = keep.flatten();
    format!("{}{tag}", &s[..keep.unwrap_or(s.len())])
}

/// `json` with the list at `pointer` `n` long: cut, or grown by copies of
/// its last item, fresh if `new.0`; the lists of the same length beside
/// it (an object's other fields) change with it, fresh if `new.1`.
fn with_len(json: &Value, pointer: &str, n: usize, new: (bool, bool)) -> Option<Value> {
    let mut json = json.clone();
    let len = json.pointer(pointer)?.as_array()?.len();
    let (parent, key) = pointer.rsplit_once('/')?;
    let resize = |list: &mut Vec<Value>, fresh_items: bool| {
        let last = list.last()?.clone();
        let copy = |k| if fresh_items { fresh(&last, k, true) } else { last.clone() };
        let grown = (list.len()..n).map(copy).collect::<Vec<_>>();
        list.truncate(n);
        list.extend(grown);
        Some(())
    };
    match json.pointer_mut(parent) {
        Some(Value::Object(map)) => {
            for (k, v) in map.iter_mut() {
                match v {
                    Value::Array(list) if list.len() == len => {
                        resize(list, if k == key { new.0 } else { new.1 })?
                    }
                    _ if k == key => return None,
                    _ => {}
                }
            }
        }
        _ => resize(json.pointer_mut(pointer)?.as_array_mut()?, new.0)?,
    }
    Some(json)
}

/// `json` with the list at `pointer` one copy of its last item longer,
/// the lists beside it untouched.
fn longer(json: &Value, pointer: &str) -> Option<Value> {
    let mut json = json.clone();
    let list = json.pointer_mut(pointer)?.as_array_mut()?;
    list.push(list.last()?.clone());
    Some(json)
}

/// `json` with the string at `pointer` `n` bytes: cut, or padded with
/// `x`.
fn with_bytes(json: &Value, pointer: &str, n: usize) -> Option<Value> {
    let mut json = json.clone();
    let Value::String(s) = json.pointer_mut(pointer)? else { return None };
    if n < s.len() {
        let cut = (0..=n).rev().find(|&c| s.is_char_boundary(c))?;
        s.truncate(cut);
    }
    let pad = n.saturating_sub(s.len());
    s.push_str(&"x".repeat(pad));
    Some(json)
}

/// `json` with the number at `pointer` set to `n`, as the witness wrote
/// it (a number, or a decimal string).
pub(crate) fn with_number(json: &Value, pointer: &str, n: u128, string: bool) -> Value {
    let mut out = json.clone();
    for at in expand(json, pointer) {
        if let Some(slot) = out.pointer_mut(&at) {
            let n = n.to_string();
            *slot = if string {
                Value::String(n)
            } else {
                serde_json::from_str(&n).unwrap_or(Value::Null)
            };
        }
    }
    out
}

/// `T` from `json`, if it deserializes.
pub(crate) fn rebuild<T: DeserializeOwned>(json: Value) -> Option<T> {
    serde_json::from_value(json).ok()
}

/// The largest of the unsigned and signed integer types' maxima that `T`
/// takes at `pointer`: its type's max, found by trying.
pub(crate) fn type_max<T: DeserializeOwned>(
    json: &Value,
    pointer: &str,
    string: bool,
) -> Option<u128> {
    let maxima = [
        u128::MAX,
        i128::MAX as u128,
        u64::MAX.into(),
        i64::MAX as u128,
        u32::MAX.into(),
        i32::MAX as u128,
        u16::MAX.into(),
        i16::MAX as u128,
        u8::MAX.into(),
        i8::MAX as u128,
    ];
    maxima.into_iter().find(|&m| rebuild::<T>(with_number(json, pointer, m, string)).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    enum Op {
        Withdraw { amount: String, min_gas: Gas, ids: Vec<String>, small: u8 },
    }

    /// An op type for acts that are calls.
    #[derive(Clone, Debug, Serialize, Deserialize)]
    struct NoOp;

    impl super::super::Op for NoOp {
        fn submit(&self, _: &mut crate::Sim) -> Result<(), super::super::Skip> {
            Ok(())
        }
    }

    /// The serde_json error text the axes read an op's variants and
    /// fields from: a serde change fails here, not as missing axes.
    #[test]
    fn serde_names_variants_and_missing_fields() {
        let unknown = serde_json::from_value::<Op>(json!({"Deposit": {}})).unwrap_err();
        assert_eq!(serde_says::variants(&unknown.to_string()), ["Withdraw"]);
        let missing = serde_json::from_value::<Op>(json!({"Withdraw": {}})).unwrap_err();
        assert_eq!(serde_says::missing_field(&missing.to_string()), Some("amount"));
    }

    /// What `args` takes for granted of a call: every edit it makes
    /// inside JSON args still reads as a call.
    #[test]
    fn a_calls_json_args_rebuild_whatever_the_edit() {
        let args = json!({ "ids": ["a", "bb"], "n": 1, "s": "7", "x": null, "o": { "k": [] } });
        let call = Call::new("alice.near", "front.near", "put").args_json(args);
        let (json, axes) = act_json::<NoOp, crate::Sim>(&Act::Call(call)).unwrap();
        // `/args` itself is the args' tag (`json`), read the usual way.
        let leaves = axes.leaves(&json).into_iter().filter(|(at, _)| at != "/args");
        let leaves: Vec<_> = leaves.collect();
        assert!(leaves.len() >= 7, "{leaves:?}");
        for (at, _) in leaves {
            assert!(at.starts_with("/args/json/"), "{at}");
            let mut edits =
                vec![with_number(&json, &at, 0, false), with_number(&json, &at, 9, true)];
            edits.extend([longer(&json, &at), with_bytes(&json, &at, 3)].into_iter().flatten());
            for t in ["0", "x"] {
                let mut j = json.clone();
                *j.pointer_mut(&at).unwrap() = Value::String(t.into());
                edits.push(j);
            }
            for j in edits {
                assert!(rebuild::<Call>(j.clone()).is_some(), "{at}: {j}");
            }
        }
    }

    fn op() -> Value {
        let ids = vec!["a".into(), "bb".into()];
        let op =
            Op::Withdraw { amount: "1000".into(), min_gas: Gas::from_teragas(40), ids, small: 3 };
        serde_json::to_value(op).unwrap()
    }

    #[test]
    fn every_number_list_and_string_is_an_axis() {
        let names = Axes::auto().leaves(&op());
        let want = [
            ("/Withdraw/amount", Leaf::Number { value: 1000, string: true }),
            ("/Withdraw/ids", Leaf::Len(2)),
            ("/Withdraw/ids/0", Leaf::Bytes(1)),
            ("/Withdraw/ids/1", Leaf::Bytes(2)),
            ("/Withdraw/min_gas", Leaf::Number { value: 40_000_000_000_000, string: false }),
            ("/Withdraw/small", Leaf::Number { value: 3, string: false }),
        ];
        let want: Vec<(String, Leaf)> = want.into_iter().map(|(p, l)| (p.to_owned(), l)).collect();
        assert_eq!(names, want);
        let only = Axes::only(["/Withdraw/ids"]).leaves(&op());
        assert_eq!(only.len(), 3, "{only:?}");
    }

    #[test]
    fn a_number_is_rebuilt_as_written_and_its_max_found_by_trying() {
        let j = op();
        let zero = with_number(&j, "/Withdraw/amount", 0, true);
        assert_eq!(zero.pointer("/Withdraw/amount"), Some(&json!("0")));
        assert!(rebuild::<Op>(zero).is_some());
        assert_eq!(type_max::<Op>(&j, "/Withdraw/small", false), Some(u8::MAX.into()));
        assert_eq!(type_max::<Op>(&j, "/Withdraw/min_gas", false), Some(u64::MAX.into()));
        // A String takes any digits.
        assert_eq!(type_max::<Op>(&j, "/Withdraw/amount", true), Some(u128::MAX));
    }

    #[test]
    fn lists_grow_fresh_and_together_and_strings_pad() {
        let j = json!({ "ids": ["a", "b"], "amounts": ["1", "1"], "to": "r.near", "one": [7] });
        let grown = with_len(&j, "/ids", 4, (true, true)).unwrap();
        assert_eq!(grown["ids"], json!(["a", "b", "b-2", "b-3"]));
        assert_eq!(grown["amounts"], json!(["1", "1", "1", "1"]));
        assert_eq!(grown["one"], json!([7]));
        assert_eq!(with_len(&j, "/ids", 1, (true, true)).unwrap()["amounts"], json!(["1"]));
        assert_eq!(with_len(&json!({ "ids": [] }), "/ids", 2, (true, true)), None);
        // Ids stay valid ids of their kind; other text ends in -k.
        assert_eq!(fresh_text("alice.near", 100), "alice-100.near");
        assert_eq!(fresh_text("nep141:ft.near", 2), "nep141:ft-2.near");
        let hex = "ab".repeat(32);
        assert_eq!(fresh_text(&hex, 0), format!("{}abababac", &hex[..56]));
        assert_eq!(fresh_text("Alice", 3), "Alice-3");
        assert_eq!(fresh_text("Tok0000001", 3), "Tok00000-3");
        // A copy keeps numbers and lone strings, and freshens list items.
        assert_eq!(fresh(&j, 5, false)["ids"], json!(["a-5", "b-5"]));
        assert_eq!(fresh(&j, 5, false)["to"], json!("r.near"));
        assert_eq!(fresh(&j, 5, false)["amounts"], json!(["1", "1"]));
        assert_eq!(with_bytes(&j, "/to", 8).unwrap()["to"], json!("r.nearxx"));
        assert_eq!(with_bytes(&j, "/to", 1).unwrap()["to"], json!("r"));
    }
}
