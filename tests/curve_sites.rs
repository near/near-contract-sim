//! Curves name a transaction's entry receipt as the rest of the crate
//! does: `receiver` for a receipt that calls nothing (a transfer), and
//! `receiver::a,b` for a batch, not `receiver::` or the first call's name.
//!
//! Verification boundary: the row sites of actual simulator runs.

use near_contract_sim::explore::{Harness, Op, Plan, Resource, Skip};
use near_contract_sim::{Balance, Fixture, Sim};
use serde::{Deserialize, Serialize};

const WAT: &str = r#"
(module
  (memory (export "memory") 1)
  (func (export "first"))
  (func (export "second")))
"#;

fn world() -> Sim {
    Fixture::new()
        .account("alice.near", Balance::from_near(100))
        .account("bob.near", Balance::from_near(1))
        .contract("app.near", wat::parse_str(WAT).unwrap())
        .build()
}

/// A transfer to `bob.near`, or a batch of `first` then `second` on
/// `app.near`, each sized by `n`.
#[derive(Clone, Debug, Serialize, Deserialize)]
enum Entry {
    Transfer { n: u64 },
    Batch { n: u64 },
}

impl Op for Entry {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        match self {
            Entry::Transfer { n } => {
                sim.tx("alice.near", "bob.near")
                    .transfer(Balance::from_yoctonear(u128::from((*n).max(1))))
                    .submit();
            }
            Entry::Batch { n } => {
                let args = n.to_le_bytes();
                sim.tx("alice.near", "app.near").call("first").args(args).call("second").submit();
            }
        }
        Ok(())
    }
}

fn sites(op: Entry, axis: &str) -> Vec<String> {
    let audit = Harness::new(world()).curve(&Plan::new().op(op), 0, axis).run();
    eprintln!("{audit}");
    audit
        .curves()
        .iter()
        .filter(|r| r.resource == Resource::PrepaidGas)
        .map(|r| r.site.clone())
        .collect()
}

#[test]
fn a_receipt_that_calls_nothing_is_named_by_its_receiver() {
    assert_eq!(sites(Entry::Transfer { n: 1 }, "n"), ["bob.near"]);
}

#[test]
fn a_batch_is_named_by_all_its_calls() {
    assert_eq!(sites(Entry::Batch { n: 1 }, "n"), ["app.near::first,second"]);
}
