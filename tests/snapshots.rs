//! `Sim::cached`: setup once per test binary, loaded from disk after that.

mod common;
use common::*;
use near_contract_sim::{
    AccessKey, Balance, GlobalContractDeployMode, GlobalContractIdentifier, KeyType, NonceMode,
    SecretKey, Sim, builtin,
};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

/// An unhandled failure, an invalid transaction, a parked yield and a
/// returned one, which keeps its transaction pending.
fn setup() -> Sim {
    let mut sim = sim_with_users();
    sim.deploy("probe.near", probe_wasm());
    sim.deploy("y.near", yield_wasm());
    sim.deploy("jar.near", tip_jar_wasm());
    sim.call("alice.near", "jar.near", "tip")
        .args(b"nobody.near".to_vec())
        .transact()
        .assert_success();
    sim.call("bob.near", "probe.near", "height")
        .deposit(near(1_000_000))
        .transact()
        .assert_failure();
    sim.call("alice.near", "probe.near", "start_yield").transact().assert_success();
    assert!(sim.call("bob.near", "y.near", "yield_return").transact().is_pending());
    sim
}

/// Everything observable after the same calls; both yields time out, and
/// the returned one's transaction gets the callback's value.
fn tail(mut sim: Sim) -> String {
    let (parked, returned) = match &sim.yields()[..] {
        [p, r] => (p.clone(), r.clone()),
        ys => panic!("{ys:?}"),
    };
    assert!(sim.outcome(returned.tx).is_pending());
    let call = sim.call("bob.near", "probe.near", "height").transact();
    sim.advance_to(returned.expires_at);
    sim.settle();
    assert!(sim.yields().is_empty());
    // The setup's unhandled failure, and nothing else: the ledger balances.
    let vs = sim.findings();
    assert_eq!(
        vs.iter().map(|v| v.check.as_str()).collect::<Vec<_>>(),
        [builtin::UNHANDLED_FAILURE]
    );
    let last: String = sim.view("probe.near", "last", ()).json();
    assert_eq!(last, "timeout");
    let returned = sim.outcome(returned.tx);
    assert_eq!(returned.json::<String>(), "timeout");
    let parked = format!("{parked:?}\n{:?}", sim.outcome(parked.tx));
    format!("{parked}\n{returned:?}\n{call:?}\n{vs:?}\n{}", sim.state_root())
}

#[test]
fn a_loaded_snapshot_continues_like_the_original() {
    // A key no earlier run used: the first call is a miss.
    let key = SystemTime::now();
    let mut runs = 0;
    let mut sims: Vec<Sim> = (0..2)
        .map(|_| {
            Sim::cached(key, || {
                runs += 1;
                setup()
            })
        })
        .collect();
    assert_eq!(runs, 1);
    let (loaded, original) = (sims.pop().unwrap(), sims.pop().unwrap());
    assert_eq!(loaded.state_root(), original.state_root());
    assert_eq!(tail(loaded), tail(original));
}

fn user(id: &str) -> Sim {
    let mut sim = Sim::new();
    sim.create_account(id, near(1_000));
    sim
}

#[test]
fn call_sites_never_share_a_snapshot() {
    let alice = Sim::cached("same key", || user("alice.near"));
    let bob = Sim::cached("same key", || user("bob.near"));
    assert!(alice.account("alice.near").is_some() && alice.account("bob.near").is_none());
    assert!(bob.account("bob.near").is_some() && bob.account("alice.near").is_none());
}

#[test]
fn concurrent_misses_run_setup_once() {
    let key = SystemTime::now();
    let runs = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..4 {
            s.spawn(|| {
                Sim::cached(key, || {
                    runs.fetch_add(1, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(300));
                    user("alice.near")
                })
            });
        }
    });
    assert_eq!(runs.into_inner(), 1);
}

#[test]
fn one_call_site_keeps_a_snapshot_per_key() {
    let t = SystemTime::now();
    let runs = std::cell::Cell::new(0);
    let fixture = |n: u8| {
        Sim::cached((t, n), || {
            runs.set(runs.get() + 1);
            user("alice.near")
        })
    };
    for n in [1, 2, 1, 2] {
        fixture(n);
    }
    assert_eq!(runs.get(), 2);
}

fn gas_key() -> SecretKey {
    SecretKey::from_seed(KeyType::ED25519, "gk")
}

/// A non-default config, the clock and gas price set later, ML-DSA and
/// secp256k1 signers, a funded gas key, a global contract used by account and
/// a signed delegate nobody relayed yet.
fn wide_setup() -> Sim {
    let mut sim = Sim::new();
    sim.config.chain_id = "mainnet".into();
    sim.config.seed = 7;
    sim.config.block_time = Duration::from_millis(600);
    sim.config.block_gas_limit = None;
    for (id, t) in [("pq.near", KeyType::MLDSA65), ("k1.near", KeyType::SECP256K1)] {
        sim.create_account_with_key(id, near(100), &SecretKey::from_seed(t, id));
    }
    sim.create_account("alice.near", near(100));
    let gk = gas_key().public_key();
    sim.tx("alice.near", "alice.near")
        .add_key(gk.clone(), AccessKey::gas_key_full_access(4))
        .transfer_to_gas_key(gk, near(5))
        .transact()
        .assert_success();
    sim.create_account("pub.near", near(1_000));
    sim.tx("pub.near", "pub.near")
        .deploy_global(status_wasm(), GlobalContractDeployMode::AccountId)
        .transact()
        .assert_success();
    sim.create_account("app.near", near(1_000));
    let id = GlobalContractIdentifier::AccountId("pub.near".parse().unwrap());
    sim.tx("app.near", "app.near").use_global(id).transact().assert_success();
    sim.set_height(sim.height() + 100);
    sim.set_timestamp_ns(1_700_000_000_000_000_000);
    sim.config.gas_price = Balance::from_yoctonear(300_000_000);
    let h = sim.height();
    sim.tx("alice.near", "app.near").transfer(Balance::from_yoctonear(1)).sign_delegate(h + 100);
    sim
}

fn wide_tail(mut sim: Sim) -> String {
    let msg = |m: &str| json!({ "message": m });
    let pq = sim.call("pq.near", "app.near", "set_status").args_json(msg("pq")).transact();
    let k1 = sim.call("k1.near", "app.near", "set_status").args_json(msg("k1")).transact();
    let gk = sim
        .call("alice.near", "app.near", "set_status")
        .args_json(msg("gk"))
        .sign_with(gas_key())
        .nonce_index(2)
        .transact();
    for o in [&pq, &k1, &gk] {
        o.assert_success();
    }
    // The delegate still holds alice's next nonce: a strict one can't skip it.
    let strict = sim
        .call("alice.near", "app.near", "set_status")
        .args_json(msg("strict"))
        .nonce_mode(NonceMode::Strict)
        .transact();
    assert!(err(&strict).contains("InvalidNonce"), "{strict}");
    let key = sim.access_key("alice.near", &gas_key().public_key()).unwrap();
    let gk_balance = key.gas_key_info().map(|g| g.balance);
    format!(
        "{pq:?}\n{k1:?}\n{gk:?}\n{strict:?}\n{gk_balance:?} {:?} {} {} {}",
        sim.config,
        sim.height(),
        sim.timestamp_ns(),
        sim.state_root()
    )
}

#[test]
fn a_loaded_snapshot_keeps_signers_gas_keys_globals_delegates_and_settings() {
    let key = SystemTime::now();
    let mut runs = 0;
    let mut sims: Vec<Sim> = (0..2)
        .map(|_| {
            Sim::cached(key, || {
                runs += 1;
                wide_setup()
            })
        })
        .collect();
    assert_eq!(runs, 1);
    let (loaded, original) = (sims.pop().unwrap(), sims.pop().unwrap());
    assert_eq!(wide_tail(loaded), wide_tail(original));
}
