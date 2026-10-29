//! Stubs: stand-in contracts with declared methods and settable replies.
//!
//! A [`Stub`] declares the methods another contract would have and what each
//! does when called ([`Reply`]): return some bytes, panic, log, burn gas.
//! [`Sim::deploy_stub`] deploys it, [`Sim::stub_reply`] changes a reply later
//! for every caller, [`Tx::stub_reply`](crate::Tx::stub_reply) sets one for
//! one transaction's calls only, and [`Sim::calls_to`] lists what any
//! account, stub or not, was asked to do.
//!
//! The stub is a real contract, run by nearcore: its wasm is generated from
//! `src/stub.wat`, one export per declared method, and its replies live in
//! its own storage. So a reply set on one `Sim` affects only that `Sim` and
//! the clones made after it, and an undeclared method fails as on chain, with
//! `MethodResolveError`. Its gas is the stub's, not the real contract's: a
//! gas measurement through a stub says nothing about the contract it stands
//! in for; [`Reply::burn_leaving`] picks how much of the attached gas is left.

use crate::chain::{self, BoundReply, action_receipt};
use crate::sim::{IntoAccountId, Sim};
use near_primitives::hash::CryptoHash;
use near_primitives::transaction::ExecutionStatus;
use near_primitives::types::{AccountId, Balance, Gas};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// The template every stub's wasm is generated from.
const TEMPLATE: &str = include_str!("stub.wat");
/// The storage key of the declared methods' names, in order (borsh).
const METHODS: &[u8] = b"stub/methods";

/// A stand-in contract: its declared methods, each with what it does when
/// called. Deploy it with [`Sim::deploy_stub`].
///
/// ```
/// # use near_contract_sim::{Reply, Stub};
/// let verifier = Stub::new()
///     .on("verify_quote", Reply::panic("TCB collateral expired"))
///     .on("version", Reply::json("1.2.0"));
/// ```
#[must_use = "a Stub does nothing until deployed: sim.deploy_stub(id, stub)"]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stub {
    methods: Vec<(String, Reply)>,
}

impl Stub {
    /// A stub with no methods.
    pub fn new() -> Self {
        Self::default()
    }

    /// Declares `method`, which replies `reply`. Declaring it again replaces
    /// its reply.
    pub fn on(mut self, method: &str, reply: Reply) -> Self {
        match self.methods.iter_mut().find(|(m, _)| m == method) {
            Some((_, r)) => *r = reply,
            None => self.methods.push((method.to_string(), reply)),
        }
        self
    }

    /// The stub's wasm: `src/stub.wat`, with one export per declared method.
    pub(crate) fn wasm(&self) -> Vec<u8> {
        let mut methods = String::new();
        let mut offset = 0;
        for (i, (method, _)) in self.methods.iter().enumerate() {
            let key = reply_key(i);
            methods += &format!(
                "(data (i32.const {offset}) \"{key}\")\n  \
                 (func (export \"{}\") (call $reply (i64.const {}) (i64.const {offset})))\n  ",
                escape(method),
                key.len(),
            );
            offset += key.len();
        }
        assert!(offset < 65536, "a stub of {} methods has too many", self.methods.len());
        let wat = TEMPLATE.replace(";; {methods}", &methods);
        wat::parse_str(&wat).expect("stub.wat compiles")
    }
}

/// What a stub's method does when called, in this order: fails if it was
/// attached too little ([`Reply::fail_below`]), logs its lines, makes its
/// call ([`Reply::call`]), burns gas if asked, then returns its bytes or
/// panics.
#[must_use = "a Reply does nothing until a stub answers with it: Stub::on(method, reply)"]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reply {
    logs: Vec<String>,
    call: Option<Box<StubCall>>,
    leave: Option<Gas>,
    burn: Option<Gas>,
    below: Option<Gas>,
    panic: bool,
    payload: Vec<u8>,
    /// The length the stub pads its payload to, with spaces, as it runs.
    fill: Option<Fill>,
}

/// What [`Reply::pad_to`] and [`Reply::at_limit`] pad to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fill {
    To(usize),
    /// `max_length_returned_data` of the sim the reply is stored in.
    Limit,
}

/// A call a stub makes before it answers: [`Reply::call`] starts one,
/// [`then`](Self::then) gives the answer.
#[derive(Clone, Debug, PartialEq, Eq)]
#[must_use = "a stub's call needs its answer: .then(reply)"]
pub struct StubCall {
    receiver: AccountId,
    method: String,
    args: Vec<u8>,
    deposit: Balance,
}

impl StubCall {
    /// Attaches `deposit` to the call (1 yoctoNEAR for a method that
    /// requires it), from the stub's own balance.
    pub fn deposit(mut self, deposit: Balance) -> Self {
        self.deposit = deposit;
        self
    }

    /// Makes the call, then answers with `answer`: its logs, gas and value
    /// or panic. A panicking answer fails the receipt, and its call with it.
    pub fn then(self, answer: Reply) -> Reply {
        Reply { call: Some(Box::new(self)), ..answer }
    }
}

impl Reply {
    /// Returns `value` serialized as JSON: `Reply::json("0")` returns `"0"`,
    /// a NEP-141 receiver keeping everything it was sent. `Reply::json(())`
    /// returns nothing, as a near-sdk method returning `()` does, not
    /// `null` (`Reply::json(None::<u8>)` is `null`).
    #[track_caller]
    pub fn json(value: impl Serialize) -> Self {
        let bytes = serde_json::to_vec(&value).expect("a reply serializes to JSON");
        // `()` (or a unit struct): void, not `null`.
        let void = std::mem::size_of_val(&value) == 0 && bytes == b"null";
        Self::bytes(if void { vec![] } else { bytes })
    }

    /// Returns `bytes` as they are, JSON or not: `Reply::bytes("{")` is
    /// malformed JSON.
    pub fn bytes(bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            logs: vec![],
            call: None,
            leave: None,
            burn: None,
            below: None,
            panic: false,
            payload: bytes.into(),
            fill: None,
        }
    }

    /// A counterparty that acts before it answers: the stub calls `method`
    /// of `receiver` with `args` as JSON, then answers as
    /// [`then`](StubCall::then) says. The call goes out with all the gas the
    /// stub leaves unused (weight 1), and as it leaves before the answer, it
    /// runs before the callback that reads the answer: a receiver that calls
    /// back into the contract mid-flow.
    ///
    /// ```
    /// # use near_contract_sim::Reply;
    /// # use serde_json::json;
    /// // Pings another contract with the args it chose, then answers.
    /// let reply = Reply::call("bob.near", "ping", json!({ "n": 1 })).then(Reply::json("answered"));
    /// ```
    ///
    /// A view can't make calls: a view of a method with one fails.
    #[track_caller]
    pub fn call(receiver: impl IntoAccountId, method: &str, args: impl Serialize) -> StubCall {
        let json = serde_json::to_vec(&args).expect("the call's args serialize to JSON");
        StubCall {
            receiver: receiver.into_account_id(),
            method: method.to_owned(),
            args: if json == b"null" { vec![] } else { json },
            deposit: Balance::ZERO,
        }
    }

    /// Panics with `message`: the call fails with
    /// `Smart contract panicked: {message}`.
    pub fn panic(message: &str) -> Self {
        Self { panic: true, ..Self::bytes(message) }
    }

    /// First burns gas until at most `gas` of the attached gas is left (a
    /// little less once it returns), as a contract that does expensive work.
    pub fn burn_leaving(self, gas: Gas) -> Self {
        Self { leave: Some(gas), ..self }
    }

    /// First burns `gas` (a little more), whatever was attached, as a
    /// contract whose work costs a fixed amount: attached less, the call
    /// runs out of gas. Before [`burn_leaving`](Self::burn_leaving), if
    /// both.
    pub fn burn(self, gas: Gas) -> Self {
        Self { burn: Some(gas), ..self }
    }

    /// Panics with `stub: attached <a> gas below <gas>` when its receipt
    /// was attached less than `gas`, before anything else: a receiver that
    /// needs `gas` to do its work and checks for it, as contracts that
    /// call on do.
    pub fn fail_below(self, gas: Gas) -> Self {
        Self { below: Some(gas), ..self }
    }

    /// Pads the bytes it returns with trailing spaces to `len` (as they
    /// are, if longer). The stub writes the padding as it runs, so only
    /// the host's limit on returned data applies, not the storage limits
    /// on a reply it reads.
    pub fn pad_to(self, len: usize) -> Self {
        Self { fill: Some(Fill::To(len)), ..self }
    }

    /// Pads the bytes it returns to the largest the host returns,
    /// `max_length_returned_data` of the sim the reply is stored in, as
    /// [`pad_to`](Self::pad_to) does. A JSON value padded with whitespace
    /// still parses.
    /// [`Shape::Limit`](crate::explore::input::Shape::Limit) answers this way.
    pub fn at_limit(self) -> Self {
        Self { fill: Some(Fill::Limit), ..self }
    }

    /// First logs `lines`, one log each, `EVENT_JSON:` events included.
    pub fn logs(self, lines: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self { logs: lines.into_iter().map(Into::into).collect(), ..self }
    }

    /// This reply, with `base`'s logs, call, gas and padding (of a value,
    /// not a panic) where it sets none: a bound reply over the stub-wide
    /// one.
    fn over(self, base: Reply) -> Reply {
        Reply {
            logs: if self.logs.is_empty() { base.logs } else { self.logs },
            call: self.call.or(base.call),
            leave: self.leave.or(base.leave),
            burn: self.burn.or(base.burn),
            below: self.below.or(base.below),
            fill: self.fill.or(base.fill.filter(|_| !self.panic)),
            ..self
        }
    }

    /// The reply [`encode`](Self::encode) wrote (a fill to the limit reads
    /// back as a fill to its length).
    fn decode(b: &[u8]) -> Option<Reply> {
        let mut c = Cursor(b);
        let panic = c.take(1)?[0] == 1;
        let (leave, burn, below) = (c.u64()?, c.u64()?, c.u64()?);
        let fill = c.u32()?;
        if below > 0 {
            // `fail_below`'s message, which `below` rebuilds.
            c.text()?;
            c.text()?;
        }
        let logs =
            (0..c.u32()?).map(|_| String::from_utf8(c.text()?).ok()).collect::<Option<_>>()?;
        let call = match c.take(1)?[0] {
            0 => None,
            _ => Some(Box::new(StubCall {
                receiver: String::from_utf8(c.text()?).ok()?.parse().ok()?,
                method: String::from_utf8(c.text()?).ok()?,
                args: c.text()?,
                deposit: Balance::from_yoctonear(u128::from_le_bytes(c.take(16)?.try_into().ok()?)),
            })),
        };
        let gas = |g: u64| (g > 0).then(|| Gas::from_gas(g));
        Some(Reply {
            logs,
            call,
            leave: (leave != u64::MAX).then(|| Gas::from_gas(leave)),
            burn: gas(burn),
            below: gas(below),
            panic,
            payload: c.0.to_vec(),
            fill: (fill > 0).then_some(Fill::To(fill)),
        })
    }

    /// The value `$reply` in `stub.wat` reads; `limit` is the largest
    /// reply the host returns, for [`at_limit`](Self::at_limit).
    fn encode(&self, limit: usize) -> Vec<u8> {
        let mut v = vec![u8::from(self.panic)];
        v.extend(self.leave.map_or(u64::MAX, |g| g.as_gas()).to_le_bytes());
        v.extend(self.burn.map_or(0, |g| g.as_gas()).to_le_bytes());
        v.extend(self.below.map_or(0, |g| g.as_gas()).to_le_bytes());
        let fill = match self.fill {
            None => 0,
            Some(Fill::To(n)) => n,
            Some(Fill::Limit) => limit,
        };
        v.extend(u32::try_from(fill).unwrap_or(u32::MAX).to_le_bytes());
        // A length-prefixed byte string.
        let put = |v: &mut Vec<u8>, bytes: &[u8]| {
            v.extend((bytes.len() as u32).to_le_bytes());
            v.extend(bytes);
        };
        if let Some(below) = self.below {
            put(&mut v, b"stub: attached ");
            put(&mut v, format!(" gas below {}", below.as_gas()).as_bytes());
        }
        v.extend((self.logs.len() as u32).to_le_bytes());
        for line in &self.logs {
            put(&mut v, line.as_bytes());
        }
        match &self.call {
            None => v.push(0),
            Some(c) => {
                v.push(1);
                for part in [c.receiver.as_bytes(), c.method.as_bytes(), &c.args] {
                    put(&mut v, part);
                }
                v.extend(c.deposit.as_yoctonear().to_le_bytes());
            }
        }
        v.extend(&self.payload);
        v
    }
}

/// Reads [`Reply::encode`]'s bytes in order.
struct Cursor<'a>(&'a [u8]);

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let (head, rest) = self.0.split_at_checked(n)?;
        self.0 = rest;
        Some(head)
    }

    fn u32(&mut self) -> Option<usize> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?) as usize)
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }

    /// A length, then that many bytes.
    fn text(&mut self) -> Option<Vec<u8>> {
        let n = self.u32()?;
        Some(self.take(n)?.to_vec())
    }
}

/// One function call a receipt made to an account: what it was asked to do,
/// by whom, and whether the receipt succeeded.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IncomingCall {
    /// The method called.
    pub method_name: String,
    /// The arguments, as sent.
    pub args: Vec<u8>,
    /// Who called: the account whose receipt or transaction sent it.
    pub predecessor_id: AccountId,
    /// Who signed the transaction it descends from.
    pub signer_id: AccountId,
    /// The deposit attached to this call.
    pub deposit: Balance,
    /// The gas attached to this call.
    pub gas: Gas,
    /// The block the receipt ran in.
    pub block_height: u64,
    /// Whether the receipt succeeded. A receipt of several calls succeeds
    /// or fails as one.
    pub succeeded: bool,
    /// The receipt's id.
    pub receipt_id: CryptoHash,
}

impl IncomingCall {
    /// The arguments parsed as JSON, as
    /// [`ReceiptOutcome::args_json`](crate::ReceiptOutcome::args_json)
    /// reads a receipt's: an `Err` naming the method, the type and the args
    /// if they don't parse as `T`.
    pub fn args_json<T: DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_slice(&self.args).map_err(|e| {
            let args = String::from_utf8_lossy(&self.args);
            serde::de::Error::custom(format!(
                "{}'s args don't parse as {}: {e}: {args:?}",
                self.method_name,
                std::any::type_name::<T>()
            ))
        })
    }
}

impl Sim {
    /// Deploys `stub` to `id` with a real deploy transaction (so it produces
    /// blocks), creating `id` with 100 NEAR if it doesn't exist and
    /// replacing any contract it has, then writes its replies to its storage.
    #[track_caller]
    pub fn deploy_stub(&mut self, id: impl IntoAccountId, stub: Stub) {
        let id = id.into_account_id();
        self.deploy(&id, stub.wasm());
        let names: Vec<&String> = stub.methods.iter().map(|(m, _)| m).collect();
        self.set_state(&id, METHODS, Some(&borsh::to_vec(&names).expect("borsh")));
        for (i, (_, reply)) in stub.methods.iter().enumerate() {
            self.set_state(&id, reply_key(i).as_bytes(), Some(&reply.encode(self.reply_limit())));
        }
    }

    /// Makes `reply` what `method` of the stub at `id` does from now on,
    /// for every caller: the stub-wide reply. A transaction that binds its
    /// own ([`Tx::stub_reply`](crate::Tx::stub_reply)) keeps it, and what
    /// that leaves unset from the stub-wide reply when it was bound. It writes
    /// the stub's storage, so it affects only this `Sim` and clones made
    /// after it. Panics if `id` is no stub, or `method` isn't one it
    /// declared (listing those it did).
    #[track_caller]
    pub fn stub_reply(&mut self, id: impl IntoAccountId, method: &str, reply: Reply) {
        let id = id.into_account_id();
        let key = self.reply_key(&id, method);
        self.set_state(&id, key.as_bytes(), Some(&reply.encode(self.reply_limit())));
    }

    /// Whether `id` holds a stub ([`deploy_stub`](Self::deploy_stub)).
    pub(crate) fn is_stub(&self, id: &AccountId) -> bool {
        self.state(id, METHODS).is_some()
    }

    /// The largest reply the host returns, `max_length_returned_data`.
    fn reply_limit(&self) -> usize {
        let limits = &self.config.runtime_config().wasm_config.limit_config;
        usize::try_from(limits.max_length_returned_data).unwrap_or(usize::MAX)
    }

    /// `reply` for `method` of the stub at `id`, to bind to a transaction.
    #[track_caller]
    pub(crate) fn bound_reply(&self, id: AccountId, method: &str, reply: &Reply) -> BoundReply {
        let key = self.reply_key(&id, method).into_bytes();
        let wide = self.state(&id, &key).and_then(|v| Reply::decode(&v));
        let reply = wide.map_or_else(|| reply.clone(), |w| reply.clone().over(w));
        let value = reply.encode(self.reply_limit());
        BoundReply { stub: id, method: method.to_string(), key, value }
    }

    /// The storage key of `method`'s reply in the stub at `id`. Panics if
    /// `id` is no stub, or `method` isn't one it declared.
    #[track_caller]
    fn reply_key(&self, id: &AccountId, method: &str) -> String {
        let names = self.state(id, METHODS).unwrap_or_else(|| {
            panic!("{id} is no stub: deploy one with Sim::deploy_stub");
        });
        let names: Vec<String> = borsh::from_slice(&names).expect("a stub's methods");
        let Some(i) = names.iter().position(|m| m == method) else {
            panic!("the stub at {id} declares no `{method}`; it declares: {}", names.join(", "));
        };
        reply_key(i)
    }

    /// Every function call to `id` that ran, in execution order, failed ones
    /// included, read from the receipt log: any account, stub or not. Views
    /// aren't receipts and don't appear.
    pub fn calls_to(&self, id: impl IntoAccountId) -> Vec<IncomingCall> {
        let id = id.into_account_id();
        let log = self.chain.log();
        let mut calls = vec![];
        for block in &log.blocks {
            for e in block.outcomes.iter().filter(|e| e.outcome.executor_id == id) {
                if log.txs.contains_key(&e.id) {
                    continue; // a conversion
                }
                // A local receipt, the signer's to itself, has no body: its
                // transaction describes it.
                let body = log.receipts.get(&e.id);
                let receipt = body.and_then(|r| action_receipt(r));
                let (predecessor, signer, actions) = match (body, &receipt) {
                    (Some(r), Some(a)) => (r.predecessor_id(), a.signer_id(), a.actions()),
                    _ => match log.converted_from(&e.id) {
                        Some(t) => (t.signer_id(), t.signer_id(), t.actions()),
                        None => continue, // a data receipt
                    },
                };
                let succeeded = matches!(
                    e.outcome.status,
                    ExecutionStatus::SuccessValue(_) | ExecutionStatus::SuccessReceiptId(_)
                );
                calls.extend(chain::calls(actions).map(|fc| IncomingCall {
                    method_name: fc.method_name.clone(),
                    args: fc.args.clone(),
                    predecessor_id: predecessor.clone(),
                    signer_id: signer.clone(),
                    deposit: fc.deposit,
                    gas: fc.gas,
                    block_height: block.height,
                    succeeded,
                    receipt_id: e.id,
                }));
            }
        }
        calls
    }
}

fn reply_key(i: usize) -> String {
    format!("reply/{i}")
}

/// `s` as the inside of a WAT string.
fn escape(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'"' | b'\\' => format!("\\{}", b as char),
            0x20..=0x7e => (b as char).to_string(),
            _ => format!("\\{b:02x}"),
        })
        .collect()
}
