//! Gas prices of another protocol on the pinned semantics
//! (`Config::gas_pricing`): the prices come from nearcore's
//! own `RuntimeConfigStore`, nothing else does, the chain's books still
//! balance, and a report made under them is labelled and never merged
//! into a default one.
//!
//! Verification boundary: executable gas-accounting inputs, which the
//! pinned runtime owns; compared here with nearcore's own config values
//! per version.

mod common;
use common::status_wasm;
use near_contract_sim::nearcore::near_parameters::{ActionCosts, ExtCosts, RuntimeConfigStore};
use near_contract_sim::{Balance, Config, Sim};
use serde_json::json;

/// Nearcore's own config of protocol `v`.
fn store(v: u32) -> std::sync::Arc<near_contract_sim::nearcore::near_parameters::RuntimeConfig> {
    RuntimeConfigStore::new(None).get_config(v).clone()
}

fn priced(v: Option<u32>) -> Config {
    let mut c = Config::default();
    c.gas_pricing = v;
    c
}

/// Protocol 81's prices are nearcore's 81 values (a first trie-node touch
/// 16.1 Ggas, 7 times 87's 2.28), action fees included; limits and
/// features stay the pinned protocol's.
#[test]
fn prices_come_from_the_store_and_nothing_else_does() {
    let (at81, at87) = (store(81), store(87));
    let touch = |c: &Config| {
        c.runtime_config().wasm_config.ext_costs.gas_cost(ExtCosts::touching_trie_node)
    };
    let cached = |c: &Config| {
        c.runtime_config().wasm_config.ext_costs.gas_cost(ExtCosts::read_cached_trie_node)
    };
    let (old, new) = (priced(Some(81)), priced(None));
    assert_eq!(touch(&old).as_gas(), 16_101_955_926);
    assert_eq!(touch(&new).as_gas(), 2_280_000_000);
    assert_eq!(cached(&old).as_gas(), 2_280_000_000);
    assert_eq!(touch(&old), at81.wasm_config.ext_costs.gas_cost(ExtCosts::touching_trie_node));
    assert_eq!(touch(&new), at87.wasm_config.ext_costs.gas_cost(ExtCosts::touching_trie_node));
    for e in [ExtCosts::storage_write_base, ExtCosts::storage_read_base, ExtCosts::base] {
        assert_eq!(
            old.runtime_config().wasm_config.ext_costs.gas_cost(e),
            at81.wasm_config.ext_costs.gas_cost(e)
        );
    }
    assert_eq!(old.runtime_config().wasm_config.regular_op_cost, at81.wasm_config.regular_op_cost);
    // Action fees: account creation changed between them.
    let create = |c: &Config| c.runtime_config().fees.fee(ActionCosts::create_account).execution;
    assert_eq!(create(&old), at81.fees.fee(ActionCosts::create_account).execution);
    assert_eq!(create(&new), at87.fees.fee(ActionCosts::create_account).execution);
    assert_ne!(create(&old), create(&new));
    // Semantics and limits are the pinned protocol's.
    let (o, n) = (&old.runtime_config().wasm_config, &new.runtime_config().wasm_config);
    assert_eq!(o.limit_config, n.limit_config);
    assert_eq!(o.reftypes_bulk_memory, n.reftypes_bulk_memory);
    assert_eq!(o.storage_get_mode, n.storage_get_mode);
    assert_eq!(o.linear_op_base_cost, n.linear_op_base_cost);
    assert_eq!(
        old.runtime_config().fees.storage_usage_config,
        new.runtime_config().fees.storage_usage_config
    );
    assert_ne!(o.limit_config.max_gas_burnt, at81.wasm_config.limit_config.max_gas_burnt);
}

/// The same write burns more under protocol 81's prices: its trie-node
/// touches cost 16.1 Ggas each. The chain's books still balance, as each
/// block's fees are read at the prices it ran with.
#[test]
fn a_write_costs_more_under_old_prices_and_the_books_balance() {
    let burnt = |pricing: Option<u32>| {
        let mut sim = Sim::new();
        sim.config.gas_pricing = pricing;
        sim.create_account("alice.near", Balance::from_near(100));
        sim.deploy("status.near", status_wasm());
        let out = sim
            .call("alice.near", "status.near", "set_status")
            .args_json(json!({ "message": "hello" }))
            .transact();
        out.assert_success();
        assert!(sim.findings().is_empty(), "{:?}", sim.findings());
        let r = out.receipts.iter().find(|r| r.method_names == ["set_status"]).unwrap();
        let touched = r.gas_profile.get("TOUCHING_TRIE_NODE").copied().unwrap_or(0);
        (r.gas_burnt.as_gas(), touched)
    };
    let (new, new_touch) = burnt(None);
    let (old, old_touch) = burnt(Some(81));
    assert!(old > new, "81: {old}, 87: {new}");
    assert!(old_touch > 0 && new_touch > 0);
    assert_eq!(old_touch % 16_101_955_926, 0, "{old_touch}");
    assert_eq!(new_touch % 2_280_000_000, 0, "{new_touch}");
}
