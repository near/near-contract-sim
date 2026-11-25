//! `Cargo.toml` pins every nearcore crate in the tree to one release with
//! `=`. nearcore's requirements between its own crates are carets, and a
//! consumer's resolver doesn't read this `Cargo.lock`: an unpinned crate
//! would move to the next prerelease or patch there, and the runtime would
//! no longer be the release's.

use std::collections::BTreeMap;

fn read(file: &str) -> String {
    std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(file)).unwrap()
}

/// `name = "version"` or `name = { version = "...", ... }` lines of
/// `[dependencies]`: the name and the version requirement.
fn dependencies(manifest: &str) -> BTreeMap<String, String> {
    let section = manifest.split("\n[dependencies]\n").nth(1).unwrap();
    let section = section.split("\n[").next().unwrap();
    let quoted = |s: &str| s.split('"').nth(1).map(str::to_string);
    section
        .lines()
        .filter(|l| !l.starts_with('#') && l.contains(" = "))
        .filter_map(|l| {
            let (name, spec) = l.split_once(" = ").unwrap();
            let version = match spec.strip_prefix('{') {
                Some(table) => quoted(table.split("version = ").nth(1)?)?,
                None => quoted(spec)?,
            };
            Some((name.to_string(), version))
        })
        .collect()
}

/// Every package of the lockfile, name and version.
fn locked() -> Vec<(String, String)> {
    let lock = read("Cargo.lock");
    lock.split("\n[[package]]\n")
        .skip(1)
        .map(|p| {
            let field = |k: &str| {
                let line = p.lines().find(|l| l.starts_with(&format!("{k} = "))).unwrap();
                line.split('"').nth(1).unwrap().to_string()
            };
            (field("name"), field("version"))
        })
        .collect()
}

#[test]
fn every_nearcore_crate_in_the_tree_is_pinned_to_one_release() {
    let deps = dependencies(&read("Cargo.toml"));
    let release = deps["near-runtime"].strip_prefix('=').expect("near-runtime pinned with =");
    // nearcore's crates are the ones published at its release's version;
    // near-sdk's and near-kit's near-* crates have versions of their own.
    let nearcore: Vec<String> = locked()
        .into_iter()
        .filter(|(name, version)| name.starts_with("near-") && version == release)
        .map(|(name, _)| name)
        .collect();
    assert!(nearcore.len() >= 20, "{nearcore:?}");
    for name in &nearcore {
        let pin = deps.get(name).map(String::as_str);
        assert_eq!(pin, Some(&*format!("={release}")), "{name} isn't pinned in Cargo.toml");
    }
    for (name, version) in deps.iter().filter(|(_, v)| v.starts_with('=')) {
        assert!(
            nearcore.contains(name),
            "{name} {version} pinned, but not in the tree at {release}"
        );
    }
}
