//! Chain parameters and block production: protocol versions, chain id and
//! validator, random seeds, ids, dropped transactions, `step` and
//! `run`.

mod common;
mod fuzz_common;
use common::*;
use fuzz_common::*;
use near_contract_sim::{
    Balance, CryptoHash, Failure, Gas, InvalidTxError, KeyType, SecretKey, Sim,
};
use serde_json::json;
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::LazyLock;

#[test]
fn chain_id_and_validator_are_visible_to_contracts() {
    for chain in ["localnet", "mainnet"] {
        let mut sim = Sim::new();
        sim.config.chain_id = chain.into();
        sim.deploy("env.near", env_wasm());
        assert_eq!(sim.view("env.near", "chain_id", ()).result.unwrap(), chain.as_bytes());
        let stake = |who: &str| {
            u128_le(&sim.view_raw("env.near", "validator_stake", who.as_bytes()).result.unwrap())
        };
        assert_eq!(stake("validator.near"), Balance::from_near(50_000_000).as_yoctonear());
        assert_eq!(stake("env.near"), 0);
        assert_eq!(
            u128_le(&sim.view("env.near", "validator_total_stake", ()).result.unwrap()),
            Balance::from_near(50_000_000).as_yoctonear()
        );
    }
}

fn u128_le(v: &[u8]) -> u128 {
    u128::from_le_bytes(v.try_into().unwrap())
}

/// Deploys the fuzz contract on c.near and parks a yield there whose
/// callback logs `env::random_seed()`. The callback's id is fixed now, when
/// the yield is created.
fn yield_logging_the_seed(sim: &mut Sim) -> CryptoHash {
    sim.deploy("c.near", fuzz_wasm());
    let log_seed = encode(&[Op::Env(3)]);
    let yielded = [Op::YieldCreate { slot: 0, args: log_seed, gas: 10 * TGAS, weight: 0 }];
    sim.call("c.near", "c.near", "run").args(encode(&yielded)).transact().hash
}

/// The callback's id and height, and the seed it logged.
fn seed_callback(sim: &Sim, y: CryptoHash) -> (CryptoHash, u64, Vec<String>) {
    let out = sim.outcome(y);
    let cb = out.receipts.iter().find(|r| r.logs.iter().any(|l| l.starts_with("e3:")));
    let cb = cb.expect("the callback ran");
    (cb.receipt_id, cb.block_height, cb.logs.clone())
}

/// A block's random seed derives from its height, not from the blocks before
/// it: the same callback, timing out in the same block, sees the same seed
/// whether the chain stepped through every height or jumped over the idle
/// ones.
#[test]
fn the_random_seed_at_a_height_is_the_same_stepped_or_jumped() {
    let mut stepped = Sim::new();
    let y = yield_logging_the_seed(&mut stepped);
    let expires_at = stepped.yields()[0].expires_at;
    let mut jumped = stepped.clone();
    while stepped.height() < expires_at {
        stepped.step();
    }
    jumped.advance_to(expires_at);
    stepped.settle();
    jumped.settle();
    assert_eq!(seed_callback(&stepped, y), seed_callback(&jumped, y));
}

/// A rewind that produces a height again gives its block a new random seed.
/// Two sims that differ only in how many blocks were produced at a height
/// before the same receipt runs there hand the contract different
/// `env::random_seed()`s.
#[test]
fn a_rewind_gives_a_height_a_new_random_seed() {
    let run = |extra_blocks: u64| {
        let mut sim = Sim::new();
        let y = yield_logging_the_seed(&mut sim);
        let h = sim.height();
        for _ in 0..extra_blocks {
            sim.step(); // empty blocks at h + 1, h + 2, ...
        }
        sim.set_height(h); // the resume runs at h and its callback at h + 1
        let resume = [Op::YieldResume { slot: 0, payload: vec![] }];
        sim.call("c.near", "c.near", "run").args(encode(&resume)).transact().assert_success();
        seed_callback(&sim, y)
    };
    let (id0, h0, seed0) = run(0);
    let (id1, h1, seed1) = run(1);
    assert_eq!(
        (id0, h0),
        (id1, h1),
        "the receipt must be the same for the comparison to mean anything"
    );
    assert_ne!(seed0, seed1);
    assert_eq!(run(0).2, seed0, "and it is deterministic");
}

/// Ids, and the random seed a contract reads, are unique and follow the
/// seed. A transaction's hash covers the last block's hash, which derives
/// from the seed and the number of blocks, not the height: the same
/// transaction (nonce included) signed later is another one, even once a
/// rewind produced the height it was signed at again, and it fails on its
/// used nonce instead of being taken for the first.
#[test]
fn ids_are_unique_and_seeded() {
    let run = |seed| {
        let mut sim = Sim::new();
        sim.config.seed = seed;
        sim.create_account("alice.near", near(10));
        sim.deploy("fail.near", failure_probe_wasm());
        let hello =
            |sim: &mut Sim| sim.call("alice.near", "fail.near", "hello").nonce(1).transact();
        let h = sim.height();
        let a = hello(&mut sim);
        sim.set_height(h - 1);
        sim.step(); // block h again, the last one as when `a` was signed
        let b = hello(&mut sim);
        assert_ne!(a.hash, b.hash);
        assert_eq!(a.json::<String>(), "hello");
        let used =
            matches!(b.failure(), Some(Failure::InvalidTx(InvalidTxError::InvalidNonce { .. })));
        assert!(used, "{b}");
        a.hash
    };
    let t1 = run(1);
    assert_eq!(run(1), t1);
    assert_ne!(run(2), t1);
    // The contract's `env::random_seed()` follows the seed too.
    let random = |seed| {
        let mut sim = Sim::new();
        sim.config.seed = seed;
        sim.create_account("alice.near", near(100));
        sim.deploy("probe.near", probe_wasm());
        sim.call("alice.near", "probe.near", "seed_arr").transact().json::<String>()
    };
    assert_eq!(random(1), random(1));
    assert_ne!(random(1), random(2));
}

/// A block's hash is fixed when the block is produced, so a new seed changes
/// only the blocks after it: a transaction signed now references the same
/// last block either way.
#[test]
fn a_new_seed_changes_only_later_blocks() {
    let mut sim = sim_with_users();
    sim.step();
    let mut reseeded = sim.clone();
    reseeded.config.seed = 7;
    let signed = |sim: &mut Sim| sim.call("alice.near", "alice.near", "m").submit();
    assert_eq!(signed(&mut reseeded), signed(&mut sim));
    sim.step();
    reseeded.step();
    assert_ne!(signed(&mut reseeded), signed(&mut sim));
}

/// nearcore gives a transaction no outcome only when the chunk's tokens
/// burnt overflow u128, and then nothing could report it: the block panics.
/// In debug builds nearcore's own assertion on the outcome count stops it
/// first; otherwise the simulator's panic names the transaction, although
/// the dropped transfer, to its own signer, has already run its receipt.
/// ML-DSA's verification surcharge is burnt, not prepaid, so at this gas
/// price each transfer costs less than u128::MAX while two burn more.
#[test]
fn a_transaction_dropped_without_an_outcome_panics() {
    let mut sim = Sim::new();
    sim.config.gas_price = Balance::from_yoctonear(6 * 10u128.pow(26));
    for id in ["a.near", "b.near"] {
        let key = SecretKey::from_seed(KeyType::MLDSA65, id);
        sim.create_account_with_key(id, Balance::from_near(340_000_000_000_000), &key);
    }
    let one = Balance::from_yoctonear(1);
    sim.tx("a.near", "b.near").transfer(one).submit();
    let dropped = sim.tx("b.near", "b.near").transfer(one).submit();
    let msg = panic_message(catch_unwind(AssertUnwindSafe(|| sim.step())));
    let want = if cfg!(debug_assertions) {
        "processing_state.outcomes.len() ==".to_string()
    } else {
        format!("dropped transaction {dropped}")
    };
    assert!(msg.contains(&want), "{msg}");
}

#[test]
fn protocol_version_selects_runtime_config() {
    // Protocol 87 removed the 30% gas reward paid to contracts; 86 still had it.
    let reward = |pv| {
        let mut sim = Sim::new();
        sim.config.protocol_version = pv;
        sim.create_account("alice.near", near(100));
        sim.deploy("status.near", status_wasm());
        let before = sim.balance("status.near");
        let out = sim
            .call("alice.near", "status.near", "set_status")
            .args_json(json!({"message": "x"}))
            .transact();
        out.assert_success();
        (sim.balance("status.near").as_yoctonear() - before.as_yoctonear(), out)
    };
    let (r87, _) = reward(87);
    let (r86, out86) = reward(86);
    assert_eq!(r87, 0);
    let fc = out86.receipt("set_status").unwrap();
    assert!(r86 > 0 && r86 < fc.tokens_burnt.as_yoctonear(), "{r86}");
}

/// A view runs at the last block's protocol version, as RPC's view of the
/// latest block does, so a new `config.protocol_version` reaches views with
/// the next block. Protocol 84 predates the `chain_id` host function.
#[test]
fn a_view_runs_at_the_last_blocks_protocol_version() {
    let mut sim = Sim::new();
    sim.deploy("env.near", env_wasm());
    sim.config.protocol_version = 84;
    assert_eq!(sim.view("env.near", "chain_id", ()).result.unwrap(), b"localnet");
    sim.step();
    let err = sim.view("env.near", "chain_id", ()).assert_failure().to_string();
    assert!(err.contains("unknown or invalid import"), "{err}");
}

/// `config.runtime_config()` and `step()` refuse a protocol version
/// the pinned nearcore can't run, `step()` before the clock moves.
/// Below MIN_SUPPORTED_PROTOCOL_VERSION the VM for old configs is gone
/// (deploy panicked deep inside near-vm-runner); above PROTOCOL_VERSION the
/// config store silently served the latest config while nightly
/// `ProtocolFeature`s gated on the higher number switched on.
#[test]
fn unsupported_protocol_versions_are_refused() {
    use near_contract_sim::nearcore::near_primitives::version::{
        MIN_SUPPORTED_PROTOCOL_VERSION, PROTOCOL_VERSION,
    };
    let at = |v| {
        let mut sim = Sim::new();
        sim.config.protocol_version = v;
        sim
    };
    for v in [MIN_SUPPORTED_PROTOCOL_VERSION - 1, PROTOCOL_VERSION + 1] {
        let want = format!("config.protocol_version {v} is not supported");
        let mut sim = at(v);
        let msg = panic_message(catch_unwind(AssertUnwindSafe(|| {
            sim.config.runtime_config();
        })));
        assert!(msg.starts_with(&want), "{msg}");
        let clock = (sim.height(), sim.timestamp_ns());
        let msg = panic_message(catch_unwind(AssertUnwindSafe(|| sim.step())));
        assert!(msg.starts_with(&want), "{msg}");
        assert_eq!((sim.height(), sim.timestamp_ns()), clock);
    }
    for v in [MIN_SUPPORTED_PROTOCOL_VERSION, PROTOCOL_VERSION] {
        at(v).step();
    }
}

/// The delayed queue alone keeps the chain busy (its receipts count in the
/// congestion info's receipt bytes): at a gas price that leaves transfers
/// no refund, nothing else is in flight while it drains.
#[test]
fn run_drains_the_delayed_queue() {
    let mut sim = Sim::new();
    sim.config.gas_price = Balance::from_yoctonear(1_000_000_000);
    sim.config.block_gas_limit = Some(Gas::from_gigagas(300));
    sim.create_account("alice.near", near(10));
    sim.create_account("bob.near", near(10));
    let one = Balance::from_yoctonear(1);
    let hs: Vec<_> =
        (0..4).map(|_| sim.tx("alice.near", "bob.near").transfer(one).submit()).collect();
    let h = sim.height();
    sim.settle();
    assert_eq!(sim.height(), h + 3, "a conversion block, then two transfers per block");
    for h in hs {
        sim.outcome(h).assert_success();
    }
    assert_eq!(sim.balance("bob.near"), near(10).checked_add(Balance::from_yoctonear(4)).unwrap());
}

/// 30 calls submitted before a `run` are converted in one block and their
/// receipts run in the next, a signer's second call included; 30 sequential
/// `transact()`s would take 60+.
#[test]
fn submitted_calls_land_in_one_block() {
    let mut sim = Sim::new();
    sim.deploy("status.near", status_wasm());
    let users: Vec<String> = (0..30).map(|i| format!("u{i}.near")).collect();
    for u in &users {
        sim.create_account(u, near(10));
    }
    let first = json!({"message": "first"});
    let first = sim.call(&users[0], "status.near", "set_status").args_json(first).submit();
    let next = sim.height() + 1;
    let hs: Vec<CryptoHash> = users
        .iter()
        .map(|u| sim.call(u, "status.near", "set_status").args_json(json!({"message": u})).submit())
        .collect();
    sim.settle();
    assert!(sim.is_idle());
    for (h, u) in hs.iter().chain([&first]).zip(users.iter().chain(&users[..1])) {
        let o = sim.outcome(h);
        o.assert_success();
        let tx = o.transaction.as_ref().unwrap();
        assert_eq!((tx.receiver_id.as_str(), tx.block_height), (u.as_str(), next));
        assert_eq!(o.receipt("set_status").unwrap().block_height, next + 1);
    }

    // The same two calls one after the other cost a block each and more.
    let h = sim.height();
    for u in &users[..2] {
        sim.call(u, "status.near", "set_status")
            .args_json(json!({"message": "y"}))
            .transact()
            .assert_success();
    }
    assert!(sim.height() - h >= 4, "{}", sim.height() - h);
    // Idle already: nothing to produce.
    let h = sim.height();
    sim.settle();
    assert_eq!(sim.height(), h);
}

/// Compiled contracts outlive the process, in `<target dir>/near-contract-sim`.
#[test]
fn compiled_contracts_are_cached_in_the_target_dir() {
    let mut sim = sim_with_users();
    sim.deploy("status.near", status_wasm());
    sim.call("alice.near", "status.near", "set_status")
        .args_json(json!({"message": "hi"}))
        .transact()
        .assert_success();
    let exe = std::env::current_exe().unwrap();
    let target = exe.ancestors().find(|d| d.join("CACHEDIR.TAG").is_file()).unwrap();
    let dir = target.join("near-contract-sim");
    assert!(std::fs::read_dir(&dir).unwrap().next().is_some(), "{dir:?}");
}

static BASE: LazyLock<Sim> = LazyLock::new(|| {
    let mut sim = sim_with_users();
    sim.deploy("status.near", status_wasm());
    sim
});

/// Counts this thread's allocations, so a test can see what a clone copies.
struct Counting;

thread_local! {
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|n| n.set(n.get() + 1));
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

/// A clone copies pointers to the block log: at 10k receipts it allocates a
/// few times per map and per account, where copying the bodies would
/// allocate for every receipt. It prints the time a clone takes.
#[test]
fn clone_at_10k_receipts() {
    let mut sim = Sim::new();
    sim.create_account("sink.near", near(1_000));
    let users: Vec<String> = (0..50).map(|i| format!("u{i}.near")).collect();
    for u in &users {
        sim.create_account(u, near(10));
    }
    let mut txs = vec![];
    for _ in 0..5 {
        for _ in 0..20 {
            for u in &users {
                txs.push(sim.tx(u, "sink.near").transfer(Balance::from_yoctonear(1)).submit());
            }
        }
        sim.settle();
    }
    // A transfer and the refund of its gas price difference each.
    let receipts: usize = txs.iter().map(|h| sim.outcome(*h).receipts.len()).sum();
    assert_eq!(receipts, 10_000);
    let (before, start) = (ALLOCATIONS.get(), std::time::Instant::now());
    for _ in 0..100 {
        std::hint::black_box(sim.clone());
    }
    let (allocations, took) = ((ALLOCATIONS.get() - before) / 100, start.elapsed() / 100);
    println!("clone at {receipts} receipts: {took:?}, {allocations} allocations");
    assert!(allocations < 1_000, "{allocations} allocations per clone");
}

/// Tests (here, threads) clone one fixture and evolve independently.
#[test]
fn shared_fixture() {
    fn status(sim: &Sim) -> Option<String> {
        let args = json!({"account_id": "alice.near"});
        sim.view("status.near", "get_status", args).json()
    }
    let set = |message: &'static str| {
        std::thread::spawn(move || {
            let mut sim = BASE.clone();
            sim.call("alice.near", "status.near", "set_status")
                .args_json(json!({ "message": message }))
                .transact()
                .assert_success();
            status(&sim)
        })
    };
    let (a, b) = (set("a"), set("b"));
    assert_eq!(a.join().unwrap().as_deref(), Some("a"));
    assert_eq!(b.join().unwrap().as_deref(), Some("b"));
    assert_eq!(status(&BASE), None);
}
