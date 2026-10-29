//! The holder-counter specimen (`tests/res/holder_counter.wat`): three
//! holders, a count of three, and every partial unstake lowers the count,
//! so the fourth unstake in a plan, from one user or several, settled or in
//! one block, traps in the callback. Seven op kinds, so a plan of varied
//! ops rarely holds four unstakes.
//!
//! The default search repeats one drawn op in about one plan in eight;
//! with it, the trap is found from generated cases alone. This runs
//! against actual simulator behavior.

mod common;
use common::manifest_dir;
use near_contract_sim::explore::{Harness, Op, Plan, Skip, plans};
use near_contract_sim::{Balance, Fixture, Sim};
use proptest::prelude::*;
use serde::{Deserialize, Serialize};

const POOL: &str = "pool.near";
const USERS: [&str; 3] = ["alice.near", "bob.near", "carol.near"];
const TRAP: &str = "pool.near::on_unstake";

fn world() -> Sim {
    let wat = std::fs::read_to_string(manifest_dir().join("tests/res/holder_counter.wat")).unwrap();
    let mut fixture = Fixture::new().contract(POOL, wat::parse_str(&wat).unwrap());
    for user in USERS {
        fixture = fixture.account(user, Balance::from_near(100));
    }
    fixture
        .setup("three holders", |sim| {
            for user in USERS {
                sim.call(user, POOL, "stake").args(10u64.to_le_bytes()).transact().assert_success();
            }
        })
        .build()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum User {
    Alice,
    Bob,
    Carol,
}

impl User {
    fn id(self) -> &'static str {
        USERS[self as usize]
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum PoolOp {
    Stake { who: User, amount: u64 },
    Unstake { who: User, amount: u64 },
    UnstakeAll { who: User },
    Transfer { from: User, to: User, amount: u64 },
    Ping { who: User },
    Claim { who: User },
    Holders { who: User },
}

impl Op for PoolOp {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        let (who, method, args) = match *self {
            PoolOp::Stake { who, amount } => (who, "stake", amount.to_le_bytes().to_vec()),
            PoolOp::Unstake { who, amount } => (who, "unstake", amount.to_le_bytes().to_vec()),
            PoolOp::UnstakeAll { who } => (who, "unstake_all", vec![]),
            PoolOp::Transfer { from, to, amount } => {
                (from, "transfer", [&amount.to_le_bytes()[..], to.id().as_bytes()].concat())
            }
            PoolOp::Ping { who } => (who, "ping", vec![]),
            PoolOp::Claim { who } => (who, "claim", vec![]),
            PoolOp::Holders { who } => (who, "holders", vec![]),
        };
        sim.call(who.id(), POOL, method).args(args).submit();
        Ok(())
    }
}

fn ops() -> impl Strategy<Value = PoolOp> {
    let who = prop_oneof![Just(User::Alice), Just(User::Bob), Just(User::Carol)];
    let amount = 1u64..=3;
    prop_oneof![
        (who.clone(), amount.clone()).prop_map(|(who, amount)| PoolOp::Stake { who, amount }),
        (who.clone(), amount.clone()).prop_map(|(who, amount)| PoolOp::Unstake { who, amount }),
        who.clone().prop_map(|who| PoolOp::UnstakeAll { who }),
        (who.clone(), who.clone(), amount).prop_map(|(from, to, amount)| PoolOp::Transfer {
            from,
            to,
            amount
        }),
        who.clone().prop_map(|who| PoolOp::Ping { who }),
        who.clone().prop_map(|who| PoolOp::Claim { who }),
        who.prop_map(|who| PoolOp::Holders { who }),
    ]
}

fn harness() -> Harness<PoolOp> {
    Harness::new(world())
}

fn unstake(i: usize) -> PoolOp {
    PoolOp::Unstake { who: [User::Alice, User::Bob, User::Carol][i % 3], amount: 1 }
}

/// `n` unstakes in one block (`blocks(0)` apart), settling after the last.
fn burst(n: usize) -> Plan<PoolOp> {
    (1..n).fold(Plan::new().op(unstake(0)), |p, i| p.blocks(0).op(unstake(i)))
}

/// Three unstakes keep the count at zero; a fourth traps in the
/// callback, in one block from several users as settled.
#[test]
fn the_fourth_unstake_traps_the_counter() {
    let harness = harness();
    harness.run(&burst(3)).assert_clean();
    for plan in [burst(4), (0..4).fold(Plan::new(), |p, i| p.op(unstake(i)))] {
        let run = harness.run(&plan);
        let trapped = run.findings().iter().any(|f| f.site.to_string().contains(TRAP));
        assert!(trapped, "{run}");
    }
}

/// The default search, two seeds, finds the trap shown (a burst of
/// unstakes, or a repeat of one).
#[test]
fn the_default_search_finds_the_trap() {
    let audit = harness().search(plans(ops())).seed(1).seeds(2).run();
    let trap = |f: &&near_contract_sim::explore::Found<PoolOp>| {
        !f.finding.injected && f.finding.site.to_string().contains(TRAP)
    };
    assert!(audit.found().iter().any(|f| trap(&f)), "{audit}");
}
