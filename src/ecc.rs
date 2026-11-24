//! External contract calls (NEP-667, protocol 89): a call to a contract's
//! reserved `__external_call` method that nobody signs. The contract
//! accepts it by returning without failing, and then pays for it: its fees,
//! the gas its run burnt and the gas it attached to the promises it made.
//! A call it refuses is rolled back and charges nobody.

use crate::chain::Verdict;
use crate::outcome::{Failure, TxOutcome};
use crate::sim::{IntoAccountId, Sim};
use near_primitives::errors::TxExecutionError;
use near_primitives::transaction::EccTransaction;
use near_primitives::types::AccountId;
use near_primitives::universal_state_init::RawStateInit;
use std::fmt;

/// An external call to send: [`Sim::external_call`].
#[must_use = "an external call does nothing until submit() or transact()"]
pub struct ExternalCall<'a> {
    sim: &'a mut Sim,
    contract_id: AccountId,
    payload: Vec<u8>,
    state_init: Option<RawStateInit>,
    forced: bool,
}

/// A call sent with [`ExternalCall::submit`], for
/// [`Sim::external_call_outcome`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SentCall(usize);

/// Why a node never included an external call: the error its checks or
/// its chunk producer's dry run found, and what the run logged before it
/// failed (none for a call refused before its run).
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ExternalCallRefusal {
    /// What its checks or its dry run failed with.
    #[serde(with = "crate::outcome::borsh_hex")]
    pub error: TxExecutionError,
    /// What the dry run logged before it failed.
    pub logs: Vec<String>,
}

impl ExternalCallRefusal {
    /// The error, classified as [`TxOutcome::failure`] classifies one.
    pub fn failure(&self) -> Failure {
        Failure::new(&self.error)
    }
}

impl fmt::Display for ExternalCallRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "refused: {}", self.error)?;
        for l in &self.logs {
            write!(f, "\n  log: {l}")?;
        }
        Ok(())
    }
}

/// What came of an external call.
#[non_exhaustive]
#[derive(Clone, Debug)]
pub enum ExternalCallOutcome {
    /// A chunk included it. [`TxOutcome::hash`] is its unique id, the
    /// call's hash without its state init and the chunk's height, and
    /// [`TxOutcome::transaction`] is the call's own outcome: accepted
    /// unless the call was [`forced`](ExternalCall::forced) in and apply
    /// refused it, when it failed and burnt nothing.
    Included(Box<TxOutcome>),
    /// The node never included it: its checks before the pool, or the chunk
    /// producer's dry run, refused it. A node's `send_tx` waits for it
    /// until it times out.
    Refused(ExternalCallRefusal),
    /// The node dropped it as a repeat of a call its contract accepted at
    /// most 1,000 blocks before (near-chain's
    /// `EXTERNAL_CALL_REPEAT_WINDOW`), whose outcome this is, as a node's
    /// `send_tx` answers.
    Repeat(Box<TxOutcome>),
    /// Waiting in the pool for a block.
    Pending,
}

impl ExternalCallOutcome {
    /// The outcome of the inclusion. Panics if the call was not included.
    #[track_caller]
    pub fn included(self) -> TxOutcome {
        match self {
            ExternalCallOutcome::Included(o) => *o,
            other => panic!("expected the external call included, it was {other}"),
        }
    }

    /// Why it was refused, if it was.
    pub fn refusal(&self) -> Option<&ExternalCallRefusal> {
        match self {
            ExternalCallOutcome::Refused(r) => Some(r),
            _ => None,
        }
    }

    /// Why it was refused, classified. Panics if it wasn't refused.
    #[track_caller]
    pub fn assert_refused(&self) -> Failure {
        match self.refusal() {
            Some(r) => r.failure(),
            None => panic!("expected the external call refused, it was {self}"),
        }
    }
}

impl fmt::Display for ExternalCallOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExternalCallOutcome::Included(o) => write!(f, "included as {}:\n{o}", o.hash),
            ExternalCallOutcome::Refused(r) => write!(f, "{r}"),
            ExternalCallOutcome::Repeat(o) => write!(f, "a repeat of {}", o.hash),
            ExternalCallOutcome::Pending => write!(f, "pending"),
        }
    }
}

impl Sim {
    /// An external contract call to `contract_id`'s `__external_call` with
    /// `payload` as its input. It goes into a chunk after the chunk's
    /// signed transactions, one call per contract per chunk, if the
    /// producer's dry run accepts it. Its run sees the chunk's parent
    /// block: `env::block_height()` is one below the chunk's, so after
    /// [`set_height(h)`](Self::set_height) the call is included in block
    /// `h` and sees `h - 1`.
    pub fn external_call(
        &mut self,
        contract_id: impl IntoAccountId,
        payload: impl Into<Vec<u8>>,
    ) -> ExternalCall<'_> {
        ExternalCall {
            sim: self,
            contract_id: contract_id.into_account_id(),
            payload: payload.into(),
            state_init: None,
            forced: false,
        }
    }

    /// What came of `sent` so far.
    pub fn external_call_outcome(&self, sent: SentCall) -> ExternalCallOutcome {
        match &self.chain.send(sent.0).verdict {
            Verdict::Pending => ExternalCallOutcome::Pending,
            Verdict::Included(u) => ExternalCallOutcome::Included(Box::new(self.outcome(u))),
            Verdict::Refused(r) => ExternalCallOutcome::Refused(r.clone()),
            Verdict::Repeat(u) => ExternalCallOutcome::Repeat(Box::new(self.outcome(u))),
        }
    }
}

impl<'a> ExternalCall<'a> {
    /// Carries a universal account's state init (NEP-655), which the call
    /// installs before its run on an account that has none yet. An
    /// initialized account ignores it, and the producer strips it. The
    /// call's unique id leaves it out.
    pub fn state_init(mut self, state_init: RawStateInit) -> Self {
        self.state_init = Some(state_init);
        self
    }

    /// Packed as a chunk producer that skips its own choices would pack
    /// it: no checks before the pool, no dry run, no repeat window and no
    /// congestion check, only the rules that make a chunk holding it
    /// invalid: the call's own validity (its size, and a protocol with
    /// external calls) and the rules on a chunk's calls together, which
    /// refuse or delay it as they would any. Apply alone decides, so a refused call is included,
    /// rolled back and charges nobody. An honest producer never packs a
    /// call its dry run refuses; whether a call can reach apply refused on
    /// the network is not something this shows.
    pub fn forced(mut self) -> Self {
        self.forced = true;
        self
    }

    /// The call as sent.
    pub fn build(&self) -> EccTransaction {
        let id = self.contract_id.clone();
        EccTransaction::new_with_state_init(id, self.payload.clone(), self.state_init.clone())
    }

    /// Sends it for the next block, without producing one.
    #[track_caller]
    pub fn submit(self) -> SentCall {
        self.send().1
    }

    /// Sends it, settles ([`Sim::settle`]) and returns what came of it.
    #[track_caller]
    pub fn transact(self) -> ExternalCallOutcome {
        let (sim, sent) = self.send();
        sim.settle();
        sim.external_call_outcome(sent)
    }

    #[track_caller]
    fn send(self) -> (&'a mut Sim, SentCall) {
        let ecc = self.build();
        let (sim, forced) = (self.sim, self.forced);
        let config = sim.config.clone();
        let sent = SentCall(sim.submit_pinned(true, |chain| chain.send_ecc(&config, ecc, forced)));
        (sim, sent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::action_receipt;
    use near_primitives::receipt::AuthorizationContext;
    use near_primitives::types::{Balance, Gas};

    fn probe() -> Vec<u8> {
        let p = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/res/ecc_probe.wat");
        wat::parse_file(p).expect("ecc_probe.wat")
    }

    /// Op 13: `method` of `target` with 30 Tgas and no weight or deposit.
    fn promise(method: &str, target: &str) -> Vec<u8> {
        let mut p = vec![13u8];
        p.extend(Gas::from_teragas(30).as_gas().to_le_bytes());
        p.extend(0u64.to_le_bytes());
        p.extend(Balance::ZERO.as_yoctonear().to_le_bytes());
        p.push(method.len() as u8);
        p.extend(method.as_bytes());
        p.extend(target.as_bytes());
        p
    }

    /// Every action receipt an accepted call's chain makes, two hops down
    /// included, is signed by the contract under the external call's
    /// authorization; a signed transaction's chain carries the signer's key.
    #[test]
    fn receipts_down_an_external_calls_chain_carry_its_authorization() {
        let mut sim = Sim::new();
        for id in ["alice.near", "probe.near", "bob.near"] {
            sim.create_account(id, Balance::from_near(100));
        }
        sim.deploy("probe.near", probe());
        sim.deploy("bob.near", probe());
        let payload = promise("fwd", "bob.near");
        let ecc = sim.external_call("probe.near", payload.clone()).transact().included();
        let call = sim.call("alice.near", "probe.near", "call").args(payload);
        let signed = call.gas(Gas::from_teragas(100)).transact();
        let alice_pk = sim.secret_key("alice.near").public_key();
        for (out, signer, auth) in [
            (&ecc, "probe.near", AuthorizationContext::ExternalContractCall),
            (&signed, "alice.near", AuthorizationContext::AccessKey(alice_pk)),
        ] {
            let log = sim.chain.log();
            let calls: Vec<_> = (out.receipts.iter())
                .filter(|r| !r.is_refund())
                .map(|r| action_receipt(&log.receipts[&r.receipt_id]).expect("an action receipt"))
                .collect();
            let methods: Vec<_> = (out.receipts.iter())
                .filter(|r| !r.is_refund())
                .map(|r| r.method_names.clone())
                .collect();
            assert!(methods.contains(&vec!["fwd".to_string()]), "{methods:?}");
            assert!(methods.contains(&vec!["pk".to_string()]), "{methods:?}");
            for a in calls {
                assert_eq!(a.signer_id().as_str(), signer);
                assert_eq!(*a.authorization_context(), auth);
            }
        }
    }
}
