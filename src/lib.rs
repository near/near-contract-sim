//! An in-process NEAR contract simulator. Its engine is nearcore's own
//! `Runtime::apply` over an in-memory trie, so fees, gas, receipts, refunds,
//! callbacks and yields are what a single-shard chain on the pinned nearcore
//! does. The guides are on
//! [GitHub](https://github.com/near/near-contract-sim/blob/main/README.md).
//!
//! A test of the auction `cargo near new auction` generates, from a test
//! crate beside it (the README's layout). The test builds the wasm with
//! cargo-near's library and runs with Rust 1.95.
//!
//! ```no_run
//! use cargo_near_build::BuildOpts;
//! use near_contract_sim::{Balance, Sim};
//! use serde_json::{Value, json};
//!
//! /// The auction's wasm, built from `../auction` by cargo-near, so the test
//! /// never runs a stale one.
//! fn auction_wasm() -> Vec<u8> {
//!     let manifest = concat!(env!("CARGO_MANIFEST_DIR"), "/../auction/Cargo.toml");
//!     let opts = BuildOpts::builder().manifest_path(manifest).no_locked(true).build();
//!     let wasm = cargo_near_build::build_with_cli(opts).expect("cargo near build");
//!     std::fs::read(wasm).unwrap()
//! }
//!
//! let mut sim = Sim::new();
//! let alice = sim.create_account("alice.near", Balance::from_near(100));
//! sim.create_account("bob.near", Balance::from_near(100));
//! sim.deploy("auction.near", auction_wasm());
//! let end_time = sim.timestamp_ns() + 60_000_000_000; // a minute on
//! sim.call("auction.near", "auction.near", "init")
//!     .args_json(json!({"end_time": end_time.to_string(), "auctioneer": "bob.near"}))
//!     .transact()
//!     .assert_success();
//!
//! let before = sim.balance(&alice);
//! let deposit = Balance::from_near(1);
//! let bid = sim.call(&alice, "auction.near", "bid").deposit(deposit).transact();
//! bid.assert_success();
//! assert!(bid.receipts.iter().all(|r| r.is_success()), "{bid}");
//!
//! let highest: Value = sim.view("auction.near", "get_highest_bid", ()).json();
//! assert_eq!(highest["bidder"], "alice.near");
//! assert_eq!(highest["bid"], deposit.as_yoctonear().to_string());
//! assert_eq!(
//!     before.as_yoctonear() - sim.balance(&alice).as_yoctonear(),
//!     deposit.as_yoctonear() + bid.tokens_burnt().as_yoctonear()
//! );
//! sim.findings().assert_none();
//! ```
//!
//! - [`Sim`]: the chain, its clock, accounts and state.
//! - [`Fixture`]: a setup written as data, built once and cloned.
//! - [`Tx`]: a real signed transaction, from [`Sim::call`] or [`Sim::tx`];
//!   [`Tx::promise_results`] delivers one as a callback on any results.
//! - [`Tx::gas_edges`], [`explore::edges`]: the edges of caller gas, or of
//!   any `u64` knob, where an outcome changes, each exact to 1.
//! - [`TxOutcome`], [`ReceiptOutcome`]: what it did, receipt by receipt;
//!   their `Display` draws the receipt tree and a gas table.
//! - [`Effects`], [`Token`]: who paid whom, in NEAR and in watched tokens.
//! - [`Tx::inject`], [`Receipts`], [`Fault`]: force a spawned call to
//!   fail, run short of gas or return other bytes, or hold a receipt back
//!   for some blocks while later ones go ahead.
//! - [`Stub`], [`Sim::calls_to`]: a contract whose answers the test picks,
//!   and the calls any account received.
//! - [`Sim::findings`], [`Finding`]: receipt failures nobody learns of,
//!   failed callbacks, and NEAR that appears or vanishes.
//! - [`explore`]: the contract's operations as an enum, checks they must
//!   keep, and primitives over them: a scan of every fault and edge, a
//!   search with shrinking, curves of what receipts use, books of what the
//!   contract owes, and a diff of two builds.
//! - [`Sim::cached`]: a setup run once per build of the test binary.
#![cfg_attr(feature = "near-kit", doc = "- [`SimTransport`]: near-kit's RPC, answered by a `Sim`.")]
//! - [`nearcore`]: the nearcore crates, for what the root doesn't re-export.
//!
//! ## The usual way to do a task
//!
//! | Task | Usually | Also |
//! |---|---|---|
//! | Set up | [`Fixture`] | `sim.create_account`, `sim.deploy`, [`Sim::update_contract_state`] |
//! | Call a method | `sim.call(signer, receiver, method).args_json(..).transact()` | [`Sim::tx`] for several actions; [`Tx::submit`], then [`Sim::outcome`], when blocks matter |
//! | Args | [`Tx::args_json`] for calls; [`Sim::view`] takes JSON | [`Tx::args`], [`Sim::view_raw`] for bytes |
//! | Deploy | `sim.deploy(id, wasm_bytes)` in setup | [`Tx::deploy`] when the deploy is under test; [`Tx::deploy_global`], [`Tx::use_global`] |
//! | Create an account | [`Sim::create_account`] | [`Tx::create_account`] when its rules are under test |
//! | Let time pass | [`Sim::settle`] until idle; [`Sim::advance_to`] for yields and timeouts | [`Sim::step`], [`Sim::advance_by`], [`Sim::pause_clock_at`], [`Sim::expire`], [`Sim::advance_epochs`] |
//! | Inspect | [`TxOutcome::effects`]; [`TxOutcome::expect_receipt`] for one receipt; `{:#}` for the tree | [`Sim::receipts_in`] for blocks |
//! | Check | `sim.findings().assert_none()` | filter on [`Finding::site`] or [`Finding::receipt_id`] |
//! | Fail a callback's branch | `tx.inject(Receipts::callback(m), Fault::PromiseFailed)` | `Fault::OutOfGas`, `Fault::Gas`, `Fault::ReturnValue`; [`Receipts::nth`] for one of several |
//! | Land receipts out of order | `tx.inject(Receipts::index(0), Fault::Hold(2))` | `Fault::HoldResult` for a callback's result; `Fault::HoldUntil(receipts)` to run it after another |
//! | Fake a callee | `sim.deploy_stub(id, Stub::new().on(m, Reply::json(..)))` | [`Sim::stub_reply`] to change one answer |
//! | Property-test | `Harness::new(world).per_op(..).search(plans(ops)).run()` | [`explore::Plan`] for one exact case; [`explore::Harness::scan`] for every fault of one flow |
//! | near-kit | `SimTransport::new(sim).near(signer)` | `with` for the `Sim` itself |
//!
//! ## Versions
//!
//! While the version is 0.x, a breaking change or a new nearcore pin moves
//! the minor version; additions and fixes move the patch. nearcore's
//! types are re-exported as they are, so a nearcore bump that changes
//! one, such as a new [`ActionErrorKind`] variant, changes this crate's
//! API with it. The CHANGELOG lists what each release changes.
#![deny(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod cache;
mod chain;
mod edges;
mod effects;
pub mod explore;
mod fault;
mod findings;
mod fixture;
mod inject;
pub mod key;
mod outcome;
mod patch;
mod render;
mod sim;
mod stub;
#[cfg(feature = "near-kit")]
mod transport;
mod trap;
mod tx;

pub use chain::{Config, GasLimits};
pub use effects::{
    Delta, Effects, NearKind, NearMove, PendingReceipt, PendingState, StorageChange, Token,
    TokenMove, Unobserved, WhichToken,
};
pub use findings::{Finding, Findings, Site, builtin};
pub use fixture::Fixture;
pub use inject::{Fault, Receipts};
pub use outcome::{Failure, ReceiptOutcome, Refund, StorageOps, TxOutcome, WrittenKey};
pub use sim::{Busy, IntoAccountId, PendingYield, Sim};
pub use stub::{IncomingCall, Reply, Stub, StubCall};
#[cfg(feature = "near-kit")]
#[cfg_attr(docsrs, doc(cfg(feature = "near-kit")))]
pub use transport::SimTransport;
pub use tx::{PromiseResult, Tx, ViewResult};

pub use near_crypto::{KeyType, PublicKey, PublicKeyHandle, SecretKey, Signature};
pub use near_primitives::account::{AccessKey, AccessKeyPermission, FunctionCallPermission};
pub use near_primitives::action::delegate::SignedDelegateAction;
pub use near_primitives::action::{Action, GlobalContractDeployMode, GlobalContractIdentifier};
pub use near_primitives::errors::{ActionErrorKind, InvalidTxError, TxExecutionError};
pub use near_primitives::hash::CryptoHash;
pub use near_primitives::state_record::StateRecord;
pub use near_primitives::transaction::{ExecutionStatus, NonceMode};
pub use near_primitives::types::{AccountId, Balance, Gas, Nonce, NonceIndex, ProtocolVersion};
pub use near_primitives::views::{AccountView, FinalExecutionStatus};

/// What an everyday test names, in one import:
/// `use near_contract_sim::prelude::*;`. The chain (`Sim`, `Fixture`,
/// `Balance`, `Gas`, `AccountId`, `CryptoHash`, keys), a transaction and
/// its outcome (`Tx`, `TxOutcome`, `Failure`, `PromiseResult`), faults
/// (`Fault`, `Receipts`), stubs (`Stub`, `Reply`), value moves (`Delta`),
/// the built-in checks (`Findings`, `builtin`) and storage keys ([`key`],
/// for `key::borsh(b"s", &id)`). `Gas` is nearcore's:
/// `Gas::from_teragas(30)` is near-sdk's `Gas::from_tgas(30)`.
///
/// [`Token`] is left out: an NFT test names NEP-171's `Token`. An audit
/// harness imports [`explore::prelude`] instead, which has this and the
/// exploration API. Proptest and serde are the test's own imports.
pub mod prelude {
    pub use crate::{
        AccountId, Balance, CryptoHash, Delta, Failure, Fault, Findings, Fixture, Gas, KeyType,
        PromiseResult, PublicKey, Receipts, Reply, SecretKey, Sim, Stub, Tx, TxOutcome, builtin,
        key,
    };
}

/// The nearcore crates the simulator is built on, for what the root doesn't
/// re-export: action structs, `RuntimeConfig` and fee parameters, `Signer`,
/// other views and errors.
pub mod nearcore {
    pub use near_crypto;
    pub use near_parameters;
    pub use near_primitives;
}
