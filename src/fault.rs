//! What the chain records of a receipt a transaction spawns, which
//! selectors ([`Receipts`](crate::Receipts)) and fit read (`Spawned`).

use crate::inject::Fault;
use near_primitives::hash::CryptoHash;
use near_primitives::types::{AccountId, Gas};

/// A receipt a transaction spawned, as it left its block: what selectors
/// match and what decides which faults fit it.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct Spawned {
    pub id: CryptoHash,
    pub receiver: AccountId,
    pub methods: Vec<String>,
    /// Its actions' kinds, as `ReceiptOutcome::actions` names them.
    pub actions: Vec<String>,
    /// The gas its calls attach, in all.
    pub attached: Gas,
    /// How many promise results it waits on.
    pub awaits: usize,
    /// Each call it waits on left in the same block and makes a function
    /// call, so `PromiseFailed` can fail it.
    pub failable: bool,
    /// A receipt it waits on left in the same block and makes no function
    /// call (a transfer, say), so its result is always empty and
    /// `ReturnValue` can only give it none. One that left an earlier block
    /// isn't seen.
    pub awaits_no_call: bool,
    /// Its result goes to a callback (it has output data receivers as it
    /// leaves its block), so `HoldResult` can hold it.
    pub sends: bool,
    /// The receivers of the receipts it waits on that left in the same
    /// block, in the order it waits on them: whether a `PromiseFailed`
    /// here fails a call to another account or one the callback's own
    /// contract made to itself. One that left an earlier block isn't seen.
    /// Read by the exploration layer's tolerable faults.
    pub awaits_from: Vec<AccountId>,
}

impl Spawned {
    /// Its site, `receiver::method` ([`crate::outcome::site`]): the
    /// receipt tree's label for a receipt that only calls.
    pub(crate) fn site(&self) -> String {
        crate::outcome::site(&self.receiver, &self.methods)
    }

    /// Whether `fault` applies here without panicking, and for `Gas`,
    /// lowers its gas: what a scan tries.
    pub(crate) fn fits(&self, fault: &Fault) -> bool {
        match fault {
            Fault::PromiseFailed => self.awaits > 0 && self.failable,
            Fault::ReturnValue(v) => self.awaits > 0 && (v.is_empty() || !self.awaits_no_call),
            Fault::OutOfGas => !self.methods.is_empty(),
            Fault::Gas(gas) => self.methods.len() == 1 && *gas > Gas::ZERO && *gas < self.attached,
            Fault::Hold(n) => *n > 0,
            Fault::HoldUntil(_) => true,
            Fault::HoldResult(n) => *n > 0 && self.sends,
        }
    }
}
