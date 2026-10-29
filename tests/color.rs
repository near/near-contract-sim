//! When the assertion helpers' panics are colored. The only test in this
//! binary, since it sets environment variables.

mod common;
use common::*;
use near_contract_sim::Sim;
use near_contract_sim::explore::{Call, Harness, Plan, Verdict};
use std::io::IsTerminal;

#[test]
fn color_follows_the_environment() {
    let mut sim = sim_with_users();
    sim.deploy("fail.near", failure_probe_wasm());
    let out = sim.call("alice.near", "fail.near", "write_then_panic").transact();
    let colored = |vars: &[(&str, &str)]| {
        // SAFETY: no other thread reads the environment.
        unsafe {
            for k in ["NO_COLOR", "CLICOLOR_FORCE", "NEXTEST"] {
                std::env::remove_var(k);
            }
            for (k, v) in vars {
                std::env::set_var(k, v);
            }
        }
        let r = std::panic::catch_unwind(|| {
            out.assert_success();
        });
        r.unwrap_err().downcast_ref::<String>().unwrap().contains("\x1b[31mFAILED: panicked: boom")
    };
    assert!(colored(&[("CLICOLOR_FORCE", "1")]));
    assert!(colored(&[("NEXTEST", "1")]));
    assert!(colored(&[("NO_COLOR", ""), ("CLICOLOR_FORCE", "1")]));
    assert!(!colored(&[("NO_COLOR", "1"), ("CLICOLOR_FORCE", "1"), ("NEXTEST", "1")]));
    let tty = std::io::stderr().is_terminal();
    assert_eq!(colored(&[("CLICOLOR_FORCE", "0")]), tty);
    assert_eq!(colored(&[]), tty);
    // Caught by a harness check, the panic is the harness error's reason
    // (a check's panic is never a finding), which is plain.
    assert!(colored(&[("CLICOLOR_FORCE", "1")]));
    let run = Harness::<Call>::new(sim)
        .always("receipt succeeded", move |_: &Sim| {
            out.assert_success();
            Ok::<(), String>(())
        })
        .run(&Plan::new());
    assert_eq!(run.verdict(), Verdict::HarnessError, "{run}");
    let message = run.reason().unwrap_or_default();
    assert!(message.contains("FAILED: panicked: boom") && !message.contains('\x1b'), "{message}");
}
