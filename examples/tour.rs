//! A tour of the API, ending with per-block latency numbers.
//!
//!     cargo run --example tour            # debug build (what `cargo test` uses)
//!     cargo run --release --example tour

use near_contract_sim::{Balance, Gas, Sim};
use serde_json::json;
use std::time::{Duration, Instant};

fn wasm(name: &str) -> Vec<u8> {
    let dir =
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR unset: run under cargo");
    std::fs::read(format!("{dir}/tests/res/{name}")).unwrap()
}

fn main() {
    let t0 = Instant::now();
    let mut sim = Sim::new();
    sim.config.chain_id = "testnet".into();
    sim.set_timestamp_ns(1_577_836_800_000_000_000); // 2020-01-01
    let alice = sim.create_account("alice.near", Balance::from_near(100));
    sim.deploy("status.near", wasm("status_message.wasm"));
    sim.deploy("xcc.near", wasm("cross_contract_high_level.wasm"));
    sim.deploy("probe.near", wasm("probe.wasm"));

    // 1. A plain call via a real signed transaction.
    let out = sim
        .call(&alice, "status.near", "set_status")
        .args_json(json!({"message": "hello"}))
        .deposit(Balance::from_millinear(1))
        .gas(Gas::from_teragas(30))
        .transact();
    out.assert_success();
    println!(
        "set_status: {:?}, total gas {} ({} receipts incl. refund), {} of its 30 Tgas left",
        out.logs(),
        out.gas_burnt(),
        out.receipts.len(),
        out.receipt("set_status").unwrap().headroom()
    );

    // 2. Views.
    let s: Option<String> =
        sim.view("status.near", "get_status", json!({"account_id": "alice.near"})).json();
    println!("get_status -> {s:?}");

    // 3. Cross-contract calls with callbacks.
    let out = sim.call(&alice, "xcc.near", "factorial").args_json(json!({"n": 5})).transact();
    let calls: Vec<_> = out.receipts.iter().filter(|r| !r.is_refund()).collect();
    println!("factorial(5) = {} via {} receipts", out.json::<u32>(), calls.len());
    for r in calls {
        println!(
            "   h={} {:<16} {:>10} gas  {:?}",
            r.block_height,
            r.method_names.join(","),
            r.gas_burnt.as_gas(),
            r.logs
        );
    }

    // 4. Yield / resume in two separate transactions.
    let y = sim.call(&alice, "probe.near", "start_yield").transact();
    println!("yields after start_yield: {:?}", sim.yields());
    sim.call(&alice, "probe.near", "resume").transact().assert_success();
    let y = sim.outcome(y.hash);
    println!("callback after resume: {:?}", y.receipt("on_resume").map(|r| &r.status));
    println!("probe.last = {}", sim.view("probe.near", "last", ()).json::<String>());

    // 5. A meta transaction: alice signs, a relayer pays for the gas.
    sim.create_account("relayer.near", Balance::from_near(10));
    sim.call(&alice, "status.near", "set_status")
        .args_json(json!({"message": "relayed"}))
        .gas(Gas::from_teragas(30))
        .relay("relayer.near")
        .assert_success();

    // 6. Several actions in one transaction, and two transactions in one
    // block: submit() queues without producing blocks.
    let paid = sim
        .tx(&alice, "status.near")
        .transfer(Balance::from_near(1))
        .call("set_status")
        .args_json(json!({"message": "paid"}))
        .submit();
    let other = sim.call(&alice, "xcc.near", "factorial").args_json(json!({"n": 3})).submit();
    sim.settle();
    let block = |h| sim.outcome(h).transaction.unwrap().block_height;
    println!("transfer + set_status and factorial(3) both in block {}", block(paid));
    assert_eq!(block(paid), block(other));

    // 7. Clock: the last block's height and time, which views see.
    let year = Duration::from_secs(365 * 86_400).as_nanos() as u64;
    sim.set_timestamp_ns(sim.timestamp_ns() + year);
    println!("probe.ts (view) = {}", sim.view("probe.near", "ts", ()).json::<u64>());
    // A yield times out in block `expires_at`, 200 blocks on: advance_to skips
    // the idle heights up to it.
    sim.call(&alice, "probe.near", "start_yield").transact().assert_success();
    let y = sim.yields().pop().unwrap();
    sim.advance_to(y.expires_at);
    sim.settle();
    println!(
        "probe.last after the timeout = {}",
        sim.view("probe.near", "last", ()).json::<String>()
    );

    // 8. The built-in checks: the tip jar's `tip` sends 1 NEAR in a detached
    // promise, so a tip to a missing account fails while the transaction
    // succeeds, and nobody learns of it but the checks.
    sim.deploy("jar.near", wat::parse_bytes(&wasm("tip_jar.wat")).unwrap().into_owned());
    let out = sim.call(&alice, "jar.near", "tip").args(b"nobody.near".to_vec()).transact();
    out.assert_success();
    println!("{out}"); // the tree marks the lost transfer UNHANDLED
    for v in sim.findings() {
        println!("[{}] {}", v.check, v.message);
    }
    println!("setup + tour: {:?}\n", t0.elapsed());

    // Per-block latency.
    let empty = stats(&(0..1000).map(|_| timed_block(&mut sim)).collect::<Vec<_>>());
    let mut busy = vec![];
    for i in 0..100 {
        sim.call(&alice, "status.near", "set_status")
            .args_json(json!({"message": format!("m{i}")}))
            .submit();
        busy.extend(timed_run(&mut sim));
    }
    let busy = stats(&busy);
    let mut xcc = vec![];
    let t = Instant::now();
    for _ in 0..20 {
        sim.call(&alice, "xcc.near", "factorial").args_json(json!({"n": 10})).submit();
        xcc.extend(timed_run(&mut sim));
    }
    let xcc = stats(&xcc);
    println!("step latency (p50 / p90 / max):");
    println!("  empty block            {empty}");
    println!("  1 set_status tx/block  {busy}");
    println!(
        "  factorial(10) blocks   {xcc}  ({:?} per factorial(10) tx end-to-end)",
        t.elapsed() / 20
    );
}

fn timed_block(sim: &mut Sim) -> Duration {
    let t = Instant::now();
    sim.step();
    t.elapsed()
}

/// `run`, one timed block at a time.
fn timed_run(sim: &mut Sim) -> Vec<Duration> {
    let mut times = vec![];
    while !sim.is_idle() {
        times.push(timed_block(sim));
    }
    times
}

fn stats(v: &[Duration]) -> String {
    let mut v: Vec<_> = v.to_vec();
    v.sort();
    let p = |q: f64| v[((v.len() - 1) as f64 * q) as usize];
    format!("{:>9.2?} / {:>9.2?} / {:>9.2?}  (n={})", p(0.5), p(0.9), v[v.len() - 1], v.len())
}
