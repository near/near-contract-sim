//! Gas prices of another protocol on the pinned semantics
//! (`Config::gas_pricing`): the prices come from nearcore's
//! own `RuntimeConfigStore`, nothing else does, and the chain's books still
//! balance.
//!
//! Verification boundary: executable gas-accounting inputs, which the
//! pinned runtime owns; compared here with nearcore's own config values
//! per version.

use near_contract_sim::nearcore::near_parameters::{ActionCosts, ExtCosts, RuntimeConfigStore};
use near_contract_sim::nearcore::near_primitives::action::UniversalStateInitAction;
use near_contract_sim::nearcore::near_primitives::universal_state_init::{
    UniversalStateInit, UniversalStateInitV1,
};
use near_contract_sim::nearcore::near_primitives::utils::derive_universal_account_id;
use near_contract_sim::{Action, Balance, Config, Sim};
use std::panic::{AssertUnwindSafe, catch_unwind};

/// Nearcore's own config of protocol `v`.
fn store(v: u32) -> std::sync::Arc<near_contract_sim::nearcore::near_parameters::RuntimeConfig> {
    RuntimeConfigStore::new().get_config(v).clone()
}

fn priced(v: Option<u32>) -> Config {
    let mut c = Config::default();
    c.gas_pricing = v;
    c
}

/// Protocol 87's prices are nearcore's 87 values (a universal state init's
/// account id derivation 8.52 Ggas, 89's 8.9; no send fee per entry, 89's
/// 3.1 Ggas), action fees included; limits and features stay the pinned
/// protocol's.
#[test]
fn prices_come_from_the_store_and_nothing_else_does() {
    let (at87, at89) = (store(87), store(89));
    let derive = |c: &Config| {
        let costs = &c.runtime_config().wasm_config.ext_costs;
        costs.gas_cost(ExtCosts::universal_state_init_to_account_id_base)
    };
    let (old, new) = (priced(Some(87)), priced(None));
    assert_eq!(derive(&old).as_gas(), 8_520_000_000);
    assert_eq!(derive(&new).as_gas(), 8_900_000_000);
    let ext = |rc: &near_contract_sim::nearcore::near_parameters::RuntimeConfig, e| {
        rc.wasm_config.ext_costs.gas_cost(e)
    };
    for e in [
        ExtCosts::universal_state_init_to_account_id_base,
        ExtCosts::universal_state_init_to_account_id_byte,
        ExtCosts::storage_write_base,
        ExtCosts::touching_trie_node,
        ExtCosts::base,
    ] {
        assert_eq!(ext(old.runtime_config(), e), ext(&at87, e));
        assert_eq!(ext(new.runtime_config(), e), ext(&at89, e));
    }
    assert_eq!(old.runtime_config().wasm_config.regular_op_cost, at87.wasm_config.regular_op_cost);
    // Action fees: a universal state init's per-entry send fee changed
    // between them.
    let entry = |c: &Config| c.runtime_config().fees.fee(ActionCosts::universal_state_init_entry);
    assert_eq!(entry(&old), at87.fees.fee(ActionCosts::universal_state_init_entry));
    assert_eq!(entry(&new), at89.fees.fee(ActionCosts::universal_state_init_entry));
    assert_ne!(entry(&old), entry(&new));
    // Semantics and limits are the pinned protocol's.
    let (o, n) = (&old.runtime_config().wasm_config, &new.runtime_config().wasm_config);
    assert_eq!(o.limit_config, n.limit_config);
    assert_eq!(o.fix_contract_loading_cost, n.fix_contract_loading_cost);
    assert_eq!(o.fix_register_input_charging, n.fix_register_input_charging);
    assert_ne!(o.fix_register_input_charging, at87.wasm_config.fix_register_input_charging);
    assert_eq!(o.linear_op_base_cost, n.linear_op_base_cost);
    assert_eq!(
        old.runtime_config().fees.storage_usage_config,
        new.runtime_config().fees.storage_usage_config
    );
    assert_ne!(
        o.limit_config.max_gas_burnt_external,
        at87.wasm_config.limit_config.max_gas_burnt_external
    );
}

/// nearcore 2.15's store holds no config before protocol 85, and answers
/// 85's for an older one, so pricing at an older version is refused rather
/// than run at 85's prices under its label.
#[test]
fn prices_older_than_the_store_are_refused() {
    let at = |v| catch_unwind(AssertUnwindSafe(|| priced(Some(v)).runtime_config().clone()));
    let err = at(84).unwrap_err();
    let msg = err.downcast_ref::<String>().unwrap();
    assert!(msg.contains("config.gas_pricing 84 is not supported"), "{msg}");
    assert!(at(85).is_ok() && at(89).is_ok());
    assert!(at(90).is_err());
}

/// The same universal state init burns less under protocol 87's prices:
/// its entries cost no send fee then. The chain's books still balance, as
/// each block's fees are read at the prices it ran with.
#[test]
fn a_state_init_costs_less_under_old_prices_and_the_books_balance() {
    let burnt = |pricing: Option<u32>| {
        let mut sim = Sim::new();
        sim.config.gas_pricing = pricing;
        sim.create_account("alice.near", Balance::from_near(100));
        let data = (0..4u8).map(|i| (vec![i], vec![i; 8])).collect();
        let access_keys = Default::default();
        let state_init =
            UniversalStateInit::V1(UniversalStateInitV1 { code: None, data, access_keys }).to_raw();
        let id = derive_universal_account_id(&state_init);
        let init = UniversalStateInitAction { state_init, deposit: Balance::from_near(1) };
        let out =
            sim.tx("alice.near", &id).action(Action::UniversalStateInit(Box::new(init))).transact();
        out.assert_success();
        assert!(sim.findings().is_empty(), "{:?}", sim.findings());
        assert_eq!(sim.view_state(&id, b"").len(), 4);
        out.tokens_burnt()
    };
    let (new, old) = (burnt(None), burnt(Some(87)));
    assert!(old < new, "87: {old}, 89: {new}");
}
