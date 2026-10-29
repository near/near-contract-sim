//! The token vault specimen as an exploration harness, shared by
//! the scan, replay and diff tests: the world, the
//! ops and the checks of plan-phase-d.md example (c).
#![allow(dead_code)]

use crate::common::res;
use near_contract_sim::explore::{Amount, Fail, Harness, Op, OpDone, Skip, amount};
use near_contract_sim::{Balance, Fixture, Sim, Token};
use proptest::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::json;

pub const TOKEN: &str = "token.near";
pub const VAULT: &str = "vault.near";

/// alice and bob with 1000 tokens each, the token and the vault, all
/// registered; token balances and vault shares watched.
pub fn vault_world(vault_wasm: &str) -> Sim {
    vault_world_with(res(vault_wasm))
}

/// A check that fails by design once any op has submitted, with `why`;
/// it passes on the fixture, where a failure is the harness's error.
pub fn fails_after_ops(why: &'static str) -> impl Fn(&Sim) -> Result<(), Fail> {
    let n = vault_world("vault.wasm").txs().len();
    move |s: &Sim| if s.txs().len() > n { Err(Fail::new(why)) } else { Ok(()) }
}

/// The same world with these vault bytes.
pub fn vault_world_with(vault_wasm: Vec<u8>) -> Sim {
    Fixture::new()
        .account("alice.near", Balance::from_near(10))
        .account("bob.near", Balance::from_near(10))
        .contract(TOKEN, res("fungible_token.wasm"))
        .init("new_default_meta")
        .args_json(json!({ "owner_id": TOKEN, "total_supply": "2000" }))
        .contract(VAULT, vault_wasm)
        .init("new")
        .args_json(json!({ "token_id": TOKEN }))
        .ft_register(TOKEN, [VAULT, "alice.near", "bob.near"])
        .ft_transfer(TOKEN, TOKEN, "alice.near", 1000)
        .ft_transfer(TOKEN, TOKEN, "bob.near", 1000)
        .token(Token::nep141(TOKEN))
        .token(Token::view(VAULT, "shares_of").supply("total_shares"))
        .build()
}

pub fn view(sim: &Sim, id: &str, method: &str, args: serde_json::Value) -> u128 {
    sim.view(id, method, args).u128()
}
pub fn ft(sim: &Sim, who: &str) -> u128 {
    view(sim, TOKEN, "ft_balance_of", json!({ "account_id": who }))
}
pub fn shares(sim: &Sim, who: &str) -> u128 {
    view(sim, VAULT, "shares_of", json!({ "account_id": who }))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum User {
    Alice,
    Bob,
}
impl User {
    pub fn id(self) -> &'static str {
        match self {
            User::Alice => "alice.near",
            User::Bob => "bob.near",
        }
    }
}

/// The vault's value-moving actions. Gas is left unpinned.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum VaultOp {
    Deposit {
        who: User,
        amount: Amount,
    },
    Withdraw {
        who: User,
        amount: Amount,
    },
    Claim {
        who: User,
    },
    /// Two deposits of `each`, two transactions in one op.
    TwoDeposits {
        who: User,
        each: u128,
    },
}

impl Op for VaultOp {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        match *self {
            VaultOp::Deposit { who, amount } => {
                let n = amount.of(ft(sim, who.id()))?;
                sim.call(who.id(), TOKEN, "ft_transfer_call")
                    .args_json(json!({ "receiver_id": VAULT, "amount": n.to_string(), "msg": "" }))
                    .deposit(Balance::from_yoctonear(1))
                    .submit();
            }
            VaultOp::Withdraw { who, amount } => {
                let n = amount.of(shares(sim, who.id()))?;
                sim.call(who.id(), VAULT, "withdraw")
                    .args_json(json!({ "amount": n.to_string() }))
                    .deposit(Balance::from_yoctonear(1))
                    .submit();
            }
            VaultOp::Claim { who } => {
                sim.call(who.id(), VAULT, "claim_rewards").submit();
            }
            VaultOp::TwoDeposits { who, each } => {
                for _ in 0..2 {
                    let args =
                        json!({ "receiver_id": VAULT, "amount": each.to_string(), "msg": "" });
                    (sim.call(who.id(), TOKEN, "ft_transfer_call").args_json(args))
                        .deposit(Balance::from_yoctonear(1))
                        .submit();
                }
            }
        }
        Ok(())
    }
}

pub fn vault_ops() -> impl Strategy<Value = VaultOp> {
    let who = || prop_oneof![Just(User::Alice), Just(User::Bob)];
    prop_oneof![
        3 => (who(), amount::any()).prop_map(|(who, amount)| VaultOp::Deposit { who, amount }),
        3 => (who(), amount::any()).prop_map(|(who, amount)| VaultOp::Withdraw { who, amount }),
        1 => who().prop_map(|who| VaultOp::Claim { who }),
    ]
}

/// always: every receipt is atomic, so this holds after every block.
fn shares_add_up(sim: &Sim) -> Result<(), String> {
    let total = view(sim, VAULT, "total_shares", json!({}));
    let sum = shares(sim, "alice.near") + shares(sim, "bob.near");
    if total == sum { Ok(()) } else { Err(format!("total_shares {total} != sum of shares {sum}")) }
}

/// at_rest: reward shares are unbacked by design, so the vault holds
/// exactly the rest.
fn vault_holds_what_it_owes(sim: &Sim) -> Result<(), String> {
    let (total, rewards) =
        (view(sim, VAULT, "total_shares", json!({})), view(sim, VAULT, "total_rewards", json!({})));
    let held = ft(sim, VAULT);
    match total.checked_sub(rewards) {
        Some(owed) if owed == held => Ok(()),
        owed => Err(format!(
            "vault holds {held} tokens, owes {owed:?} (shares {total}, rewards {rewards})"
        )),
    }
}

/// per-op: what this withdraw's own receipts did.
fn withdraw_delivers(d: &OpDone<VaultOp>) -> Result<(), Fail> {
    let VaultOp::Withdraw { who, .. } = *d.op() else { unreachable!("withdraws only") };
    let taken = -d.effects().token_net(VAULT, who.id())?;
    let paid = d.effects().token_net(TOKEN, who.id())?;
    if taken != paid {
        return Err(
            format!("{} gave up {taken} shares and received {paid} tokens", who.id()).into()
        );
    }
    Ok(())
}

/// per-op: a deposit credits as many shares as it took tokens.
fn deposit_credits(d: &OpDone<VaultOp>) -> Result<(), Fail> {
    let VaultOp::Deposit { who, .. } = *d.op() else { unreachable!("deposits only") };
    let paid = -d.effects().token_net(TOKEN, who.id())?;
    let credited = d.effects().token_net(VAULT, who.id())?;
    if paid != credited {
        return Err(format!("{} paid {paid} tokens for {credited} shares", who.id()).into());
    }
    Ok(())
}

fn is_withdraw(op: &VaultOp) -> bool {
    matches!(op, VaultOp::Withdraw { .. })
}

fn is_deposit(op: &VaultOp) -> bool {
    matches!(op, VaultOp::Deposit { .. })
}

/// The vault's checks on `world`.
pub fn harness(world: Sim) -> Harness<VaultOp> {
    Harness::new(world)
        .always("total_shares is the sum of shares", shares_add_up)
        .at_rest("the vault holds what it owes", vault_holds_what_it_owes)
        .per_op_for(is_withdraw, "withdraw delivers what it debits", withdraw_delivers)
        .per_op_for(is_deposit, "deposit credits what it takes", deposit_credits)
}

/// The vault harness on the build `wasm` (in tests/res).
pub fn vault_on(wasm: &str) -> Harness<VaultOp> {
    harness(vault_world(wasm))
}
