//! The fuzz fixture's build script still builds a wasm the runtime runs.

use near_contract_sim::Sim;
use std::{path::Path, process::Command};

#[test]
fn the_rebuilt_fixture_runs_on_the_runtime() {
    let dir =
        std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR unset: run under cargo");
    let script = Path::new(&dir).join("tests/contracts/fuzz-contract/build.sh");
    let output = std::env::temp_dir().join(format!("ncs-fixture-{}", std::process::id()));
    let result = Command::new("bash")
        .arg(script)
        .arg(&output)
        // Cargo sets these for the native tests; the fixture owns its toolchain.
        .env_remove("RUSTUP_TOOLCHAIN")
        .env_remove("CARGO")
        .output()
        .expect("run the fuzz fixture's build script");
    assert!(
        result.status.success(),
        "fixture build failed:\n{}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    let wasm = std::fs::read(output.join("fuzz_contract.wasm")).expect("built fixture");
    std::fs::remove_dir_all(output).unwrap();
    let mut sim = Sim::new();
    sim.deploy("fuzz.near", wasm);
    let digest = sim.view("fuzz.near", "digest", ()).result.unwrap();
    let digest: Vec<Option<Vec<u8>>> = borsh::from_slice(&digest).unwrap();
    assert!(digest.len() == 8 && digest.iter().all(Option::is_none));
}
