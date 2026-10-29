//! The docs hold compiled code: every `rust` block of the README and the
//! agent skill, whose `references/` holds the guides, appears in code
//! cargo compiles (`src/`, `examples/`, or `tests/` outside the contract
//! crates under `tests/contracts/`), and every `wat` block in a `.wat`
//! file. A block that can't be is marked on the line before its fence,
//! `<!-- not compiled: why -->`. Every fence names one of the languages
//! these checks know, so none skips them. Their links name files.
//! `tests/consumer/run.sh` runs the quickstart's blocks and the first
//! harness under the quickstart's own dependencies, and the README's test
//! in a fresh consumer; other fragments run here. `tests/consumer/*.rs`
//! counts as compiled code, though only `run.sh` compiles it.

mod common;
use common::*;
use near_contract_sim::{ActionErrorKind, Balance, Failure, Gas, Sim};
use serde_json::json;
use std::path::Path;

/// Where rustdoc links the guides, since the crate isn't on docs.rs.
const BLOB: &str = "https://github.com/near/near-contract-sim/blob/main/";

#[test]
fn every_block_is_compiled_code() {
    for (lang, ext) in [("rust", "rs"), ("wat", "wat")] {
        let code = ["src", "tests", "examples"].map(|d| sources(&manifest_dir().join(d), ext));
        // A line no source has between files, so no block matches across two.
        let code: Vec<String> = code.concat().iter().map(|file| lines(file)).collect();
        let code = format!("\n{}", code.join("\0\n"));
        for (page, text) in pages() {
            for (why, block) in marked_blocks(&text, lang) {
                if let Some(why) = why {
                    assert!(!why.trim().is_empty(), "{page}: a block is marked with no reason");
                    continue;
                }
                let found = code.contains(&format!("\n{}", lines(block)));
                assert!(found, "{page}: this {lang} block is compiled nowhere:\n{block}");
            }
        }
    }
}

/// The crate root's example is `tests/consumer/sim.rs`, which `run.sh` runs
/// in a fresh consumer, less its `#[test]` wrapper.
#[test]
fn the_crate_example_is_the_consumer_test() {
    let read = |path: &str| std::fs::read_to_string(manifest_dir().join(path)).unwrap();
    let doc: String = read("src/lib.rs")
        .lines()
        .filter_map(|l| l.strip_prefix("//!"))
        .map(|l| l.to_string() + "\n")
        .collect();
    let example = doc.split("```no_run\n").nth(1).unwrap().split("```").next().unwrap();
    let test = read("tests/consumer/sim.rs").replace("#[test]\nfn bid() {\n", "");
    let test = test.trim_end().strip_suffix('}').unwrap();
    assert_eq!(lines(example), lines(test));
}

/// The docs' `near-kit` dependency is `Cargo.toml`'s version, and the
/// features they turn on are `Cargo.toml`'s.
#[test]
fn toml_blocks_match_cargo_toml() {
    let manifest = std::fs::read_to_string(manifest_dir().join("Cargo.toml")).unwrap();
    let quoted = |l: &str| l.split('"').nth(1).unwrap().to_string();
    let version = quoted(manifest.lines().find(|l| l.starts_with("near-kit = {")).unwrap());
    let features = manifest.split("\n[features]").nth(1).unwrap().split("\n[").next().unwrap();
    let mut near_kit_lines = 0;
    for (page, text) in pages() {
        for line in blocks(&text, "toml").into_iter().flat_map(str::lines) {
            if line.starts_with("near-kit = ") {
                assert_eq!(quoted(line), version, "{page}: {line}");
                near_kit_lines += 1;
            }
            if let Some((_, list)) = line.split_once("features = [")
                && line.starts_with("near-contract-sim = ")
            {
                for f in list.split(']').next().unwrap().split(", ") {
                    let known = features.contains(&format!("\n{} = [", f.trim_matches('"')));
                    assert!(known, "{page}: {line}");
                }
            }
        }
    }
    assert!(near_kit_lines > 0, "no near-kit line in the docs' toml blocks");
}

/// A link from a page to a file names one that exists, and so does a link
/// into this repo from a page or from rustdoc. No link has an anchor.
#[test]
fn links_name_files() {
    let exists = |path: &Path| manifest_dir().join(path).exists();
    for (page, text) in pages() {
        let prose: String = text.split("```").step_by(2).collect();
        for target in prose.split("](").skip(1).map(|t| t.split(')').next().unwrap()) {
            assert!(!target.contains('#'), "{page}: {target} has an anchor");
            let path = match target.strip_prefix(BLOB) {
                Some(path) => Path::new(path).to_path_buf(),
                None if target.starts_with("http") => continue,
                None => Path::new(&page).parent().unwrap().join(target),
            };
            assert!(exists(&path), "{page}: {target} names no file");
        }
    }
    let rustdoc = sources(&manifest_dir().join("src"), "rs").concat();
    for path in rustdoc.split(BLOB).skip(1).map(|t| t.split(')').next().unwrap()) {
        assert!(!path.contains('#') && exists(Path::new(path)), "src: {BLOB}{path}");
    }
}

/// Every fence of the README and the skill names a language these checks
/// know: a `rust,ignore`, `rs` or bare opening fence would skip them.
#[test]
fn fences_name_known_languages() {
    const KNOWN: [&str; 5] = ["rust", "wat", "toml", "sh", "text"];
    for (page, text) in pages() {
        let fences = text.lines().map(str::trim_start).filter(|l| l.starts_with("```"));
        for info in fences.step_by(2).map(|l| l.trim_start_matches('`').trim()) {
            assert!(KNOWN.contains(&info), "{page}: a fence opens as ```{info}");
        }
    }
}

/// The concept pages: each quotes compiled code, so each says something a
/// test runs.
const CONCEPTS: [&str; 11] = [
    "sim",
    "migrating",
    "transactions",
    "blocks-and-clock",
    "outcomes-and-effects",
    "faults",
    "stubs",
    "harness",
    "explore",
    "quickstart",
    "mpc",
];

#[test]
fn every_concept_page_has_a_compiled_block() {
    for name in CONCEPTS {
        let path = format!("skills/near-contract-sim/references/{name}.md");
        let text = std::fs::read_to_string(manifest_dir().join(&path)).unwrap_or_default();
        assert!(!blocks(&text, "rust").is_empty(), "{path}: no rust block, or no page");
    }
}

/// The skill links every page under `references/`, so an agent that reads
/// it can reach them all.
#[test]
fn skill_links_every_reference() {
    let skill = std::fs::read_to_string(manifest_dir().join("skills/near-contract-sim/SKILL.md"));
    let skill = skill.unwrap();
    let dir = manifest_dir().join("skills/near-contract-sim/references");
    for entry in std::fs::read_dir(dir).unwrap() {
        let name = entry.unwrap().file_name().into_string().unwrap();
        if name.ends_with(".md") {
            let link = format!("](references/{name})");
            assert!(skill.contains(&link), "SKILL.md doesn't link references/{name}");
        }
    }
}

/// The docs budget: the skill is at most 2,500 lines. What an agent reads
/// before its first test (SKILL.md and the quickstart) is at most 450
/// lines, and before its first audit harness (SKILL.md, and audit.md up
/// to step 3) at most 600.
#[test]
fn the_docs_fit_their_budget() {
    let dir = manifest_dir().join("skills/near-contract-sim");
    let count = |path: &Path| std::fs::read_to_string(path).unwrap().lines().count();
    let refs = std::fs::read_dir(dir.join("references")).unwrap();
    let total: usize =
        count(&dir.join("SKILL.md")) + refs.map(|e| count(&e.unwrap().path())).sum::<usize>();
    assert!(total <= 2_500, "the skill is {total} lines");
    let skill = count(&dir.join("SKILL.md"));
    let everyday = skill + count(&dir.join("references/quickstart.md"));
    assert!(everyday <= 450, "{everyday} lines before a first test");
    let audit = std::fs::read_to_string(dir.join("references/audit.md")).unwrap();
    let first_page = audit[..audit.find("## 3. ").expect("step 3")].lines().count();
    assert!(skill + first_page <= 600, "{} lines before a first harness", skill + first_page);
}

/// One form of the dependency: the quickstart and the README add the same
/// `[dev-dependencies]`, pinned by `rev`, and no page puts the simulator
/// under `[dependencies]`.
#[test]
fn the_dependency_has_one_form() {
    let deps = |page: &str| -> String {
        let text = std::fs::read_to_string(manifest_dir().join(page)).unwrap();
        let block = blocks(&text, "toml").into_iter().find(|b| b.contains("near-contract-sim = "));
        let block = block.unwrap_or_else(|| panic!("{page}: no dependency block"));
        let table = block.split("\n\n").find(|t| t.contains("near-contract-sim = ")).unwrap();
        table.trim_end().to_owned()
    };
    let quickstart = deps("skills/near-contract-sim/references/quickstart.md");
    assert!(quickstart.starts_with("[dev-dependencies]\n"), "{quickstart}");
    let rev = quickstart.split(", rev = \"").nth(1).and_then(|r| r.split_once("\" }"));
    let rev = rev.unwrap_or_else(|| panic!("no rev: {quickstart}")).0;
    let commit = (7..=40).contains(&rev.len())
        && rev.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    assert!(rev == "<commit>" || commit, "rev {rev:?} is no commit: {quickstart}");
    assert_eq!(deps("README.md"), quickstart);
    for (page, text) in pages() {
        for block in blocks(&text, "toml") {
            let mut table = "";
            for line in block.lines() {
                if line.starts_with('[') {
                    table = line;
                }
                if line.starts_with("near-contract-sim = ") {
                    assert_eq!(table, "[dev-dependencies]", "{page}: {line}");
                }
            }
        }
    }
}

/// Every `.md` file at the root and in the agent skill: its path and text.
fn pages() -> Vec<(String, String)> {
    let mut pages = vec![];
    for dir in ["", "skills/near-contract-sim", "skills/near-contract-sim/references"] {
        for entry in std::fs::read_dir(manifest_dir().join(dir)).unwrap() {
            let path = Path::new(dir).join(entry.unwrap().file_name());
            if path.extension().is_some_and(|e| e == "md") {
                let text = std::fs::read_to_string(manifest_dir().join(&path)).unwrap();
                pages.push((path.display().to_string(), text));
            }
        }
    }
    pages
}

/// The code blocks of `page` fenced as `lang`.
fn blocks<'a>(page: &'a str, lang: &str) -> Vec<&'a str> {
    marked_blocks(page, lang).into_iter().map(|(_, block)| block).collect()
}

/// The code blocks of `page` fenced as `lang`, each with why it isn't
/// compiled when the line before its fence says, `<!-- not compiled: why -->`.
fn marked_blocks<'a>(page: &'a str, lang: &str) -> Vec<(Option<&'a str>, &'a str)> {
    let fence = format!("```{lang}\n");
    let parts: Vec<&str> = page.split(fence.as_str()).collect();
    let why = |before: &'a str| {
        let line = before.trim_end().lines().last()?.trim();
        line.strip_prefix("<!-- not compiled:")?.strip_suffix("-->")
    };
    let blocks = parts.iter().skip(1).map(|b| b.split("```").next().unwrap());
    parts.iter().map(|&before| why(before)).zip(blocks).collect()
}

/// The text of every `.{ext}` file under `dir`, but in a crate of its own
/// (a contract under `tests/contracts/`), which `cargo test` doesn't build.
fn sources(dir: &Path, ext: &str) -> Vec<String> {
    let mut texts = vec![];
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let crate_of_its_own = path.join("Cargo.toml").exists();
        if path.is_dir() && !path.ends_with("target") && !crate_of_its_own {
            texts.extend(sources(&path, ext));
        } else if path.extension().is_some_and(|e| e == ext) {
            texts.push(std::fs::read_to_string(&path).unwrap());
        }
    }
    texts
}

/// Each line with its whitespace runs collapsed, so indentation and
/// alignment don't matter. Blank lines are left out, and so are
/// `#[ignore = ..]` ones: a block can quote a whole test that is ignored.
fn lines(s: &str) -> String {
    let words = s.lines().map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "));
    words.filter(|l| !l.is_empty() && !l.starts_with("#[ignore")).map(|l| l + "\n").collect()
}

/// The README's cross-contract call: near-sdk's cross-contract example.
#[test]
fn cross_contract_calls() {
    let mut sim = sim_with_users();
    sim.deploy("factorial.near", xcc_wasm());
    let out =
        sim.call("alice.near", "factorial.near", "factorial").args_json(json!({"n": 3})).transact();
    assert_eq!(out.json::<u32>(), 6);
    // factorial(3) calls factorial(2), which calls factorial(1); callbacks multiply on the way back.
    let callbacks = out.receipts.iter().filter(|r| r.method_names == ["factorial_mult"]);
    assert!(callbacks.count() == 2 && out.receipts.iter().all(|r| r.is_success()), "{out}");
}

/// A near-sdk method also takes its arguments as a JSON array, in the
/// order it declares them (migrating.md).
#[test]
fn positional_json_args() {
    let mut sim = sim_with_users();
    sim.deploy("status.near", status_wasm());
    let set = sim.call("alice.near", "status.near", "set_status").args_json(json!(["hi"]));
    set.transact().assert_success();
    let get = sim.view("status.near", "get_status", json!(["alice.near"]));
    assert_eq!(get.json::<Option<String>>().as_deref(), Some("hi"));
}

/// The README's clock, on probe.near (`tests/res/probe_src.rs.txt`).
#[test]
fn the_clock() {
    let mut sim = sim_with_users();
    sim.deploy("probe.near", probe_wasm());
    sim.set_timestamp_ns(1_577_836_800_000_000_000); // 2020-01-01, or any time, the past included
    sim.advance_to(sim.height() + 7 * 86_400); // a week of 1 s blocks: 604,800 blocks
    assert_eq!(sim.timestamp_ns(), 1_577_836_800_000_000_000 + 7 * 86_400 * 1_000_000_000);

    sim.call("alice.near", "probe.near", "start_yield").transact().assert_success();
    sim.advance_to(sim.yields()[0].expires_at); // the yield times out
    sim.settle();
    assert_eq!(sim.view("probe.near", "last", ()).json::<String>(), "timeout");
}

/// Attribution follows receipts: a relayed transfer's deposit shows from
/// the sender, though the relayer paid it.
#[test]
fn a_relayed_deposit_shows_from_the_sender() {
    let mut sim = sim_with_users();
    sim.create_account("relayer.near", Balance::from_near(10));
    let (alice, relayer) = (sim.balance("alice.near"), sim.balance("relayer.near"));
    let out =
        sim.tx("alice.near", "bob.near").transfer(Balance::from_near(1)).relay("relayer.near");
    out.assert_success();
    assert_eq!(out.effects().near_paid("alice.near", "bob.near"), Balance::from_near(1));
    assert_eq!(sim.balance("alice.near"), alice); // she paid nothing
    assert!(sim.balance("relayer.near") < relayer.saturating_sub(Balance::from_near(1)));
}

/// A callback's gas margin, and a total within a tolerance.
#[test]
fn gas_assertions() {
    let mut sim = sim_with_users();
    sim.deploy("factorial.near", xcc_wasm());
    let out =
        sim.call("alice.near", "factorial.near", "factorial").args_json(json!({"n": 2})).transact();
    out.assert_headroom("factorial_mult", Gas::from_teragas(1));

    sim.deploy("status.near", status_wasm());
    let out = sim
        .call("alice.near", "status.near", "set_status")
        .args_json(json!({"message": "hi"}))
        .transact();
    fn within(got: Gas, want: Gas, tolerance: f64) -> bool {
        (got.as_gas() as f64 / want.as_gas() as f64 - 1.0).abs() <= tolerance
    }
    assert!(within(out.gas_burnt(), Gas::from_gigagas(1850), 0.1), "{out}");
}

/// The most gas a transaction can attach is the runtime config's: 1 PGas,
/// though `call` attaches 300 Tgas by default.
#[test]
fn the_gas_a_transaction_can_attach() {
    use near_contract_sim::InvalidTxError;
    use near_contract_sim::nearcore::near_primitives::errors::ActionsValidationError;
    let mut sim = sim_with_users();
    sim.deploy("status.near", status_wasm());
    let set = |sim: &mut Sim, gas| {
        let args = json!({ "message": "hi" });
        sim.call("alice.near", "status.near", "set_status").args_json(args).gas(gas).transact()
    };
    let limits = &sim.config.runtime_config().wasm_config.limit_config;
    let cap = limits.max_total_prepaid_gas;
    assert_eq!(cap, Gas::from_teragas(1_000));
    set(&mut sim, cap).assert_success();
    let over = set(&mut sim, cap.checked_add(Gas::from_gas(1)).unwrap()).assert_failure();
    let Failure::InvalidTx(InvalidTxError::ActionsValidation(e)) = over else { panic!("{over}") };
    assert!(matches!(e, ActionsValidationError::TotalPrepaidGasExceeded { .. }));
}

#[test]
fn a_storage_value_and_the_balance_above_the_stake() {
    let mut sim = sim_with_users();
    sim.deploy("status.near", status_wasm());
    let set = |sim: &mut Sim, who, message| {
        sim.call(who, "status.near", "set_status")
            .args_json(json!({ "message": message }))
            .transact()
    };
    set(&mut sim, "alice.near", "hi").assert_success();
    let state = sim.state("status.near", b"STATE");
    assert!(state.is_some());

    let a = sim.account("status.near").unwrap();
    let staked = sim.storage_cost(a.storage_usage);
    let spare = a.amount.saturating_add(a.locked).saturating_sub(staked);
    assert!(spare > Balance::ZERO && a.storage_usage > 770);
    sim.set_balance("status.near", staked);
    let failure = set(&mut sim, "bob.near", "no room").failure();
    assert!(matches!(failure, Some(Failure::Action(ActionErrorKind::LackBalanceForState { .. }))));
}

/// A contract without source (sim.md): a `.wat` deployed as it is, raw
/// args, and its storage as what the test asserts.
#[test]
fn a_contract_without_source() {
    let mut sim = sim_with_users();
    let wat = format!("{}/tests/res/guestbook.wat", env!("CARGO_MANIFEST_DIR"));
    sim.deploy("guestbook.near", wat::parse_file(wat).unwrap());
    sim.call("alice.near", "guestbook.near", "sign").args("hello").transact().assert_success();
    assert_eq!(sim.state("guestbook.near", b"galice.near"), Some(b"hello".to_vec()));
    let missing = sim.call("alice.near", "guestbook.near", "read").transact();
    assert!(missing.to_string().contains("sign, sign_paid"), "{missing}");
}

/// Callee stubs in WAT: a contract needs no memory.
#[test]
fn stub_contracts_in_wat() {
    let stubs = wat::parse_str(
        r#"(module
             (func (export "noop"))
             (func (export "trap") unreachable)
             (func (export "burn") (loop (br 0))))"#,
    )
    .unwrap();
    let mut sim = sim_with_users();
    sim.deploy("stub.near", stubs);
    let mut call = |m| sim.call("alice.near", "stub.near", m).gas(Gas::from_teragas(10)).transact();
    call("noop").assert_success();
    let trap = "WebAssembly trap: An `unreachable` opcode was executed.";
    assert_eq!(call("trap").failure(), Some(Failure::Host(trap.into())));
    assert_eq!(call("burn").failure(), Some(Failure::OutOfGas));
}

/// nearcore's host functions as views (`tests/res/host_probe.wat`), fed
/// with near-crypto's keys and signatures.
#[test]
fn host_functions_as_views() {
    use near_contract_sim::{KeyType, SecretKey};
    let mut sim = Sim::new();
    let probe = std::env::var("CARGO_MANIFEST_DIR").unwrap() + "/tests/res/host_probe.wat";
    sim.deploy("host.near", wat::parse_file(probe).unwrap());
    let host =
        |method, input: &[&[u8]]| sim.view_raw("host.near", method, input.concat()).result.unwrap();
    // A signature's bytes, without the borsh tag of its key type.
    let sign = |sk: &SecretKey, msg: &[u8]| borsh::to_vec(&sk.sign(msg)).unwrap()[1..].to_vec();

    let hash = host("keccak256", &[b"hello"]);
    assert_eq!(hash, unhex("1c8aff950685c2ed4bc3174f3472287b56d9517b9c948127319a09a7a36deac8"));
    let k1 = SecretKey::from_seed(KeyType::SECP256K1, "k1");
    assert_eq!(host("ecrecover", &[&hash, &sign(&k1, &hash)]), k1.public_key().key_data());
    let ed = SecretKey::from_seed(KeyType::ED25519, "ed");
    let (sig, pk) = (sign(&ed, b"hi"), ed.public_key());
    assert_eq!(host("ed25519_verify", &[&sig, pk.key_data(), b"hi"]), [1]);
    assert_eq!(host("ed25519_verify", &[&sig, pk.key_data(), b"ho"]), [0]);

    // Each curve's generator G: G = G, and G - G is the point at infinity.
    let g = unhex(concat!(
        "17f1d3a73197d7942695638c4fa9ac0fc3688c4f9774b905",
        "a14e3a3f171bac586c55e83ff97a1aeffb3af00adb22c6bb",
        "08b3f481e3aaa0f1a09e30ed741d8ae4fcf5e095d5d00af6",
        "00db18cb2c04b3edd03cc744a2888ae40caa232946c5e7e1",
    ));
    let infinity = [&[0x40][..], &[0; 95]].concat();
    assert_eq!(host("bls12381_p1_sum", &[&[0], &g]), g);
    assert_eq!(host("bls12381_p1_sum", &[&[0], &g, &[1], &g]), infinity);
    let g = [&[1][..], &[0; 31], &[2], &[0; 31]].concat(); // (1, 2), little-endian
    assert_eq!(host("alt_bn128_g1_sum", &[&[0], &g]), g);
    assert_eq!(host("alt_bn128_g1_sum", &[&[0], &g, &[1], &g]), [0; 64]);
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

/// The public API is `tests/api/public.txt`: every nameable item, field,
/// variant and method, with its declaration, as `tests/api/list.py` reads
/// them from rustdoc's HTML with every feature on. A change to it is a
/// CHANGELOG line. cargo-mutants skips it (`.cargo/mutants.toml`): no
/// mutant changes the API, and it runs `cargo doc`.
#[test]
fn the_public_api_is_the_committed_list() {
    // The target dir this test was built in: <target>/<profile>/deps/docs-*.
    let exe = std::env::current_exe().unwrap();
    let target = exe.ancestors().nth(3).unwrap();
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let status = std::process::Command::new(cargo)
        .args(["doc", "--no-deps", "--all-features", "--quiet", "--target-dir"])
        .arg(target)
        .current_dir(manifest_dir())
        .status()
        .expect("run cargo doc");
    assert!(status.success(), "cargo doc failed");
    let listed = std::process::Command::new("python3")
        .arg("-I")
        .arg(manifest_dir().join("tests/api/list.py"))
        .arg(target.join("doc/near_contract_sim"))
        .output()
        .expect("run python3 tests/api/list.py");
    assert!(listed.status.success(), "{}", String::from_utf8_lossy(&listed.stderr));
    let now = String::from_utf8(listed.stdout).unwrap();
    let path = manifest_dir().join("tests/api/public.txt");
    if std::env::var_os("NCS_API_WRITE").is_some() {
        std::fs::write(&path, &now).unwrap();
    }
    let committed = std::fs::read_to_string(&path).unwrap_or_default();
    let gone: Vec<_> = committed.lines().filter(|l| !now.lines().any(|n| n == *l)).collect();
    let new: Vec<_> = now.lines().filter(|l| !committed.lines().any(|c| c == *l)).collect();
    assert!(
        gone.is_empty() && new.is_empty(),
        "the public API changed.\nGone:\n  {}\nNew:\n  {}\n\nIf you meant it, rewrite \
         tests/api/public.txt with\n  NCS_API_WRITE=1 cargo test --test docs \
         the_public_api_is_the_committed_list\nand add a line to CHANGELOG.md's \
         Unreleased entry: Breaking for a removed or changed item, Added for a new one.",
        gone.join("\n  "),
        new.join("\n  "),
    );
}
