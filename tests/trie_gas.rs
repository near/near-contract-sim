//! Trie-node gas and the shape of the trie.
//!
//! `storage_write` and `storage_remove` look up the old value through the
//! trie and charge 2.28 Ggas per trie node on the way (`TOUCHING_TRIE_NODE`,
//! or `READ_CACHED_TRIE_NODE` if the chunk already touched it, in the same
//! receipt or an earlier one: the same price since protocol 82).
//! `storage_read` goes through flat storage and charges no trie nodes. A
//! value written earlier in the same chunk is found without touching the
//! trie. So the same write costs more gas in a deeper trie. Contract data of
//! every account lives under one trie column: what matters is how many
//! accounts hold contract data (the path to the contract's subtree) and how
//! much state the contract has near the key (the path inside it), not how
//! many accounts exist.
//!
//! `Sim::apply_state_records` gives the simulator a mainnet-shaped trie (see
//! `fuzz_common::mainnet_like_ids`); the differential test applies the same
//! records to a sandbox and gets identical gas.

mod common;
mod fuzz_common;
use common::{ft_wasm, status_wasm};
use fuzz_common::*;
use near_contract_sim::{Balance, Sim, StateRecord, TxOutcome};
use serde_json::json;

const TOUCH: &str = "TOUCHING_TRIE_NODE";
const CACHED: &str = "READ_CACHED_TRIE_NODE";
/// Protocol 87 and 88 gas per trie node (touched or cached).
const NODE_GAS: u64 = 2_280_000_000;

/// Gas of the non-refund receipts `executor` ran for this tx, and the
/// trie-node part of it.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Cost {
    gas: u64,
    touch: u64,
    cached: u64,
}

fn cost(o: &TxOutcome, executor: &str) -> Cost {
    o.assert_success();
    let mut c = Cost { gas: 0, touch: 0, cached: 0 };
    for r in o.receipts.iter().filter(|r| !r.is_refund() && r.receiver_id.as_str() == executor) {
        c.gas += r.gas_burnt.as_gas();
        c.touch += r.gas_profile.get(TOUCH).copied().unwrap_or(0);
        c.cached += r.gas_profile.get(CACHED).copied().unwrap_or(0);
    }
    c
}

/// Token-map key (`a` + borsh account id) of an implicit account, the most
/// common kind of holder.
fn holder_key(i: u64) -> Vec<u8> {
    let mut k = b"a".to_vec();
    k.extend(borsh::to_vec(&implicit(i)).unwrap());
    k
}

fn implicit(i: u64) -> String {
    use near_contract_sim::nearcore::near_primitives::hash::hash;
    hex(&hash(format!("implicit{i}").as_bytes()).0)
}

/// A sim with `accounts` mainnet-like accounts (every 10th holding a little
/// contract data), `c.near` (fuzz contract) holding `keys` token-like
/// entries, `ft.near` (near-sdk FT) with `keys` registered holders, and
/// `status.near`. The measured keys are token-map entries of implicit
/// accounts, the most common kind on mainnet.
fn world(accounts: usize, keys: usize) -> Sim {
    let mut sim = Sim::new();
    let ids = mainnet_like_ids(accounts, "pop");
    sim.apply_state_records(account_records(&ids));
    sim.apply_state_records(contract_state_records(&ids));
    for u in ["alice.near", "bob.near"] {
        sim.create_account(u, Balance::from_near(100));
    }
    let (ha, hb) = (implicit(1), implicit(2));
    sim.create_account(&ha, Balance::from_near(100));
    sim.create_account(&hb, Balance::from_near(100));
    sim.apply_state_records(big_contract_records("c.near", &fuzz_wasm(), keys));
    let prog: Vec<Op> =
        (0..8).map(|i| Op::Write { key: holder_key(i), value: vec![i as u8; 16] }).collect();
    sim.call("alice.near", "c.near", "run").args(encode(&prog)).transact().assert_success();
    sim.deploy("status.near", status_wasm());
    sim.call("alice.near", "status.near", "set_status")
        .args_json(json!({"message": "hi"}))
        .transact()
        .assert_success();
    sim.deploy("ft.near", ft_wasm());
    sim.call("ft.near", "ft.near", "new_default_meta")
        .args_json(json!({"owner_id": "alice.near", "total_supply": "1000000"}))
        .transact()
        .assert_success();
    for h in [&ha, &hb] {
        sim.call(h, "ft.near", "storage_deposit")
            .args_json(json!({"account_id": h}))
            .deposit(Balance::from_millinear(125))
            .transact()
            .assert_success();
    }
    sim.call("alice.near", "ft.near", "ft_transfer")
        .args_json(json!({"receiver_id": ha, "amount": "1000"}))
        .deposit(Balance::from_yoctonear(1))
        .transact()
        .assert_success();
    // Holders: the same `LookupMap` layout the contract used for `ha`.
    let me = borsh::to_vec(&ha).unwrap();
    let state = sim.view_state("ft.near", b"");
    let (k, v) = state.iter().find(|(k, _)| k.ends_with(&me)).expect("holder balance entry");
    assert_eq!(v.len(), 16);
    let prefix = k[..k.len() - me.len()].to_vec();
    let holders = mainnet_like_ids(keys, "holder");
    sim.apply_state_records(holders.iter().map(|h| StateRecord::Data {
        account_id: "ft.near".parse().unwrap(),
        data_key: [prefix.clone(), borsh::to_vec(h).unwrap()].concat().into(),
        value: 7u128.to_le_bytes().to_vec().into(),
    }));
    sim
}

/// The measured calls.
fn measure(sim: &mut Sim) -> Vec<(&'static str, Cost)> {
    let run = |sim: &mut Sim, p: Vec<Op>| {
        cost(&sim.call("alice.near", "c.near", "run").args(encode(&p)).transact(), "c.near")
    };
    let mut out = vec![
        ("fuzz: 1 storage_read", run(sim, vec![Op::Read { key: holder_key(0) }])),
        (
            "fuzz: 1 storage_write",
            run(sim, vec![Op::Write { key: holder_key(0), value: vec![9; 16] }]),
        ),
        (
            "fuzz: 8 storage_write",
            run(
                sim,
                (0..8).map(|i| Op::Write { key: holder_key(i), value: vec![3; 16] }).collect(),
            ),
        ),
    ];
    let o = sim
        .call("alice.near", "status.near", "set_status")
        .args_json(json!({"message": "hello"}))
        .transact();
    out.push(("status_message: set_status", cost(&o, "status.near")));
    let o = sim
        .call(implicit(1).as_str(), "ft.near", "ft_transfer")
        .args_json(json!({"receiver_id": implicit(2), "amount": "1"}))
        .deposit(Balance::from_yoctonear(1))
        .transact();
    out.push(("fungible_token: ft_transfer", cost(&o, "ft.near")));
    out
}

/// Records written by `apply_state_records` are what `create_account`
/// writes: same state root.
#[test]
fn account_records_match_create_account() {
    let ids = mainnet_like_ids(50, "eq");
    let mut a = Sim::new();
    a.apply_state_records(account_records(&ids));
    let mut b = Sim::new();
    for id in &ids {
        b.create_account(id.as_str(), Balance::from_near(5));
    }
    assert_eq!(a.state_root(), b.state_root());
    assert_eq!(a.account(ids[0].as_str()), b.account(ids[0].as_str()));
}

/// Contract + data records give a working contract with that state.
#[test]
fn big_contract_records_are_callable() {
    let mut sim = Sim::new();
    sim.create_account("alice.near", Balance::from_near(100));
    sim.apply_state_records(big_contract_records("big.near", &fuzz_wasm(), 2_000));
    assert_eq!(sim.view_state("big.near", b"").len(), 2_000);
    let key = big_contract_holder_key("big.near", 2_000, 17);
    let o = sim.call("alice.near", "big.near", "run").args(encode(&[Op::Read { key }])).transact();
    o.assert_success();
    assert_eq!(o.logs(), vec![format!("r{}", hex(&(1_000_000u128 * 18).to_le_bytes()))]);
}

/// A populated trie changes only the trie-node part of the gas, upward,
/// and for a write to a key among thousands of similar ones it does change.
/// Reads don't: `storage_read` charges no trie nodes.
#[test]
fn populated_trie_costs_more_trie_node_gas() {
    let base = measure(&mut world(0, 0));
    let big = measure(&mut world(2_000, 4_000));
    for ((label, a), (_, b)) in base.iter().zip(&big) {
        assert!(b.touch + b.cached >= a.touch + a.cached, "{label}: {a:?} -> {b:?}");
        assert_eq!(b.gas - (b.touch + b.cached), a.gas - (a.touch + a.cached), "{label}");
    }
    assert_eq!((base[0].1.touch, base[0].1.cached, big[0].1.touch, big[0].1.cached), (0, 0, 0, 0));
    for i in [1, 4] {
        let ((label, a), (_, b)) = (base[i], big[i]);
        assert!(b.gas > a.gas, "{label}: {a:?} -> {b:?}");
    }
}

/// On a trie forked along a key's path (`explore::trie_siblings`, as a
/// margin's shaper plants it): a write of the key touches 2 to 4 nodes
/// more per byte of its path; a read touches none, polluted or not (flat
/// storage). So a read-only receipt's gas never depends on the trie.
#[test]
fn a_polluted_path_costs_writes_trie_nodes_and_reads_none() {
    let wasm = wat::parse_str(
        std::fs::read_to_string(common::manifest_dir().join("tests/res/nonce_box.wat")).unwrap(),
    )
    .unwrap();
    let boxed: near_contract_sim::AccountId = "nonces.near".parse().unwrap();
    let key = |n: u64| [&b"n"[..], &n.to_le_bytes()].concat();
    let make = |polluted: bool| {
        let mut sim = Sim::new();
        sim.create_account("alice.near", Balance::from_near(100));
        sim.create_account(boxed.clone(), Balance::from_near(100));
        sim.deploy(boxed.clone(), wasm.clone());
        sim.set_state(boxed.clone(), &key(7), Some(b"n"));
        if polluted {
            let mut planted = near_contract_sim::explore::trie_siblings(&boxed, &key(7));
            planted.extend(near_contract_sim::explore::trie_siblings(&boxed, &key(8)));
            sim.apply_state_records(planted.into_iter().map(|(account_id, key)| {
                StateRecord::Data { account_id, data_key: key.into(), value: vec![1].into() }
            }));
        }
        sim
    };
    let nodes = |sim: &mut Sim, nonce: u64| {
        let out = sim.call("alice.near", boxed.clone(), "use").args(nonce.to_le_bytes()).transact();
        let profile = out.receipts.iter().filter(|r| r.receiver_id == "nonces.near");
        let gas =
            |r: &near_contract_sim::ReceiptOutcome, k| r.gas_profile.get(k).copied().unwrap_or(0);
        profile.map(|r| gas(r, TOUCH) + gas(r, CACHED)).sum::<u64>() / NODE_GAS
    };
    let (mut clean, mut polluted) = (make(false), make(true));
    // Nonce 7 is used: `use` reads its key and panics, writing nothing.
    assert_eq!((nodes(&mut clean, 7), nodes(&mut polluted, 7)), (0, 0));
    // Nonce 8 is new: `use` writes "n" + 8 under "nonces.near,": 21 bytes.
    let (c, p) = (nodes(&mut clean, 8), nodes(&mut polluted, 8));
    assert!(p >= c + 30, "polluted {p} nodes, clean {c}");
    eprintln!("a write of a 9-byte key: {c} trie nodes clean, {p} polluted");
}
