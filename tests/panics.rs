//! Misuse panics at the caller's line: the public functions that panic on
//! their arguments are `#[track_caller]`, and panic outside any closure. Each
//! prints once, except in an explore op, whose skipped case says where it
//! panicked instead. A binary of its own, since it swaps the
//! process's panic hook.

use near_contract_sim::explore::{Call, Faults, Harness, Op, Plan, Skip, Verdict};
use near_contract_sim::{Balance, CryptoHash, Fault, Gas, Receipts, Sim};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::panic::{self, AssertUnwindSafe};
use std::sync::{Arc, Mutex};

type Case = (&'static str, Box<dyn FnOnce(&mut Sim)>);

#[test]
fn misuse_panics_at_the_callers_line() {
    let cases: Vec<Case> = vec![
        ("an invalid id", Box::new(|s| _ = s.account("Not An Id"))),
        ("balance", Box::new(|s| _ = s.balance("nobody.near"))),
        ("set_balance", Box::new(|s| s.set_balance("nobody.near", Balance::ZERO))),
        ("set_state", Box::new(|s| s.set_state("nobody.near", b"k", None))),
        ("view_state", Box::new(|s| _ = s.view_state("nobody.near", b""))),
        ("secret_key", Box::new(|s| _ = s.secret_key("nobody.near"))),
        ("create_account", Box::new(|s| _ = s.create_account("alice.near", Balance::ZERO))),
        ("no key to sign", Box::new(|s| _ = s.call("nobody.near", "alice.near", "m").transact())),
        ("a call edit", Box::new(|s| _ = s.tx("alice.near", "alice.near").args(vec![]))),
        ("view args", Box::new(|s| _ = s.view("alice.near", "m", not_json()))),
        ("args_json", Box::new(|s| _ = s.call("alice.near", "a.near", "m").args_json(not_json()))),
        (
            "relay",
            Box::new(|s| _ = s.call("alice.near", "a.near", "m").nonce_index(0).relay("a.near")),
        ),
        ("outcome", Box::new(|s| _ = s.outcome(CryptoHash::default()))),
        ("advance_to", Box::new(|s| s.advance_to(0))),
        (
            "assert_success",
            Box::new(|s| _ = s.call("alice.near", "a.near", "m").transact().assert_success()),
        ),
        ("json", Box::new(|s| _ = s.call("alice.near", "a.near", "m").transact().json::<u8>())),
        ("a plan step with no op", Box::new(|_| _ = Plan::<Call>::new().gas(Gas::from_teragas(1)))),
        ("no faults", Box::new(|_| _ = Faults::new(Receipts::all(), []))),
        (
            "a failing run, whose op's panics don't print",
            Box::new(|s| {
                let run = Harness::new(s.clone()).run(&Plan::new().op(Misuse::ActionPanic));
                run.assert_clean();
            }),
        ),
        #[cfg(feature = "near-kit")]
        ("near", Box::new(|s| _ = near_contract_sim::SimTransport::new(s.clone()).near("x.near"))),
        (
            "runtime_config (last: it breaks the sim)",
            Box::new(|s| {
                s.config.protocol_version = 1;
                _ = s.config.runtime_config();
            }),
        ),
    ];
    let misuses = [Misuse::InvalidId, Misuse::MissingAccount, Misuse::ActionPanic];
    let printed = Arc::new(Mutex::new(vec![]));
    let seen = printed.clone();
    panic::set_hook(Box::new(move |info| {
        seen.lock().unwrap().push(info.location().map(|l| l.file().to_string()));
    }));
    let printed = || std::mem::take(&mut *printed.lock().unwrap());
    let mut sim = Sim::new();
    sim.create_account("alice.near", Balance::from_near(10));
    sim.step();
    let got: Vec<(&str, bool, Vec<Option<String>>)> = cases
        .into_iter()
        .map(|(what, f)| {
            let panicked = panic::catch_unwind(AssertUnwindSafe(|| f(&mut sim))).is_err();
            (what, panicked, printed())
        })
        .collect();
    let violations = misuses.map(|misuse| {
        let run = Harness::new(Sim::new()).run(&Plan::new().op(misuse));
        // A panic in a written plan's op is a harness error, its reason
        // where it panicked and the op (a search's case is skipped).
        assert_eq!(run.verdict(), Verdict::HarnessError, "{run}");
        (misuse, run.reason().unwrap().to_owned(), printed())
    });
    drop(panic::take_hook());
    // A hold of 0 is an error the plan keeps, listed before the first
    // case, not a panic.
    let hold0 = Plan::new().op(call()).inject(Receipts::all(), Fault::Hold(0));
    assert_eq!(hold0.errors().len(), 1, "{:?}", hold0.errors());
    for (what, panicked, files) in got {
        assert!(panicked, "{what}: no panic");
        assert_eq!(files, [Some(file!().to_string())], "{what}");
    }
    for (what, message, files) in violations {
        assert!(message.contains("Op::submit panicked"), "{what:?}: {message}");
        assert!(message.ends_with(&format!(": {what:?})")), "{what:?}: {message}");
        assert!(message.contains(concat!("panicked at ", file!(), ":")), "{what:?}: {message}");
        assert_eq!(files, [], "{what:?}");
    }
}

/// An op that misuses the simulator in its `submit`.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
enum Misuse {
    InvalidId,
    MissingAccount,
    ActionPanic,
}

impl Op for Misuse {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        match self {
            Misuse::InvalidId => _ = sim.account("Not An Id"),
            Misuse::MissingAccount => _ = sim.balance("nobody.near"),
            Misuse::ActionPanic => panic!("invalid test action"),
        }
        Ok(())
    }
}

fn call() -> Call {
    Call::new("alice.near", "a.near", "m")
}

/// Args that don't serialize: JSON has no map with tuple keys.
fn not_json() -> BTreeMap<(u8, u8), u8> {
    BTreeMap::from([((0, 0), 0)])
}
