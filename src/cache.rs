//! What persists across processes: compiled contracts and
//! `Sim::cached` snapshots.

use crate::chain::{Chain, Config, GasLimits, Log};
use crate::sim::Sim;
use borsh::{BorshDeserialize, BorshSerialize};
use near_crypto::PublicKey;
use near_primitives::hash::CryptoHash;
use near_primitives::types::{AccountId, BlockHeight, Gas, Nonce};
use near_vm_runner::{
    ContractRuntimeCache, FilesystemContractRuntimeCache, noop_background_spawner,
};
use std::collections::HashMap;
use std::fs::File;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io;
use std::panic::Location;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

/// Where compiled contracts and snapshots persist across
/// processes and runs: the first of `NCS_CACHE_DIR`, `near-contract-sim` in
/// cargo's target directory (`CARGO_TARGET_DIR` if it's absolute, or else the
/// nearest ancestor of the running executable with a `CACHEDIR.TAG`) and
/// `~/.cache/near-contract-sim` that exists or can be created. An explicit
/// `NCS_CACHE_DIR` must be private: the cache holds native code, which other
/// users must not be able to plant.
pub(crate) fn cache_dir() -> &'static Path {
    static DIR: LazyLock<PathBuf> = LazyLock::new(|| {
        let target = std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from);
        // A relative one is from where cargo ran, which the test doesn't know.
        let target = target.filter(|dir| dir.is_absolute()).or_else(|| {
            let exe = std::env::current_exe().ok()?;
            Some(exe.ancestors().find(|d| d.join("CACHEDIR.TAG").is_file())?.to_path_buf())
        });
        let home = std::env::home_dir().map(|h| h.join(".cache/near-contract-sim"));
        std::env::var_os("NCS_CACHE_DIR")
            .map(PathBuf::from)
            .into_iter()
            .chain(target.map(|dir| dir.join("near-contract-sim")))
            .chain(home)
            .find(|dir| std::fs::create_dir_all(dir).is_ok())
            .expect("a cache directory (set NCS_CACHE_DIR)")
    });
    &DIR
}

/// Compiled contracts in `cache_dir()`, plus nearcore's in-memory cache of
/// *loaded* contracts, shared by every `Sim` in the process: without it,
/// every function call maps its code afresh (a memfd and an mmap), most of
/// a receipt-heavy test's time. nearcore keys each entry by code hash and VM
/// config, and writes it to a temporary file that it then renames, so
/// parallel test processes can share the directory.
pub(crate) fn contract_cache() -> Box<dyn ContractRuntimeCache> {
    static CACHE: LazyLock<FilesystemContractRuntimeCache> = LazyLock::new(|| {
        FilesystemContractRuntimeCache::with_memory_cache(
            cache_dir(),
            None::<&str>,
            "contracts",
            // Up to 64 loaded contracts in memory (nearcore sizes it x4).
            16,
            None,
            FilesystemContractRuntimeCache::MAX_DISK_CACHE_BYTES,
            noop_background_spawner(),
        )
        .expect("compiled-contract cache")
    });
    Box::new(CACHE.clone())
}

#[derive(BorshSerialize, BorshDeserialize)]
struct Snapshot {
    entries: Vec<(Vec<u8>, Vec<u8>)>,
    root: CryptoHash,
    #[borsh(serialize_with = "write_config", deserialize_with = "read_config")]
    config: Config,
    height: u64,
    timestamp_ns: u64,
    log: Log,
    /// Each account's secret key (`SecretKey` has no borsh form).
    keys: HashMap<AccountId, String>,
    delegates: HashMap<(AccountId, PublicKey, Nonce), BlockHeight>,
    /// The block time `resume_clock` restores, while the clock is paused:
    /// (seconds, nanoseconds).
    paused: Option<(u64, u32)>,
    /// Whether the height and the time are pinned for the next
    /// transaction (`Sim::set_height`, `Sim::set_timestamp_ns`).
    pinned: (bool, bool),
    /// The methods that ran as views (`Sim::viewed`): a scan's coverage
    /// counts them, so a loaded fixture covers what the built one did.
    viewed: Vec<String>,
}

/// `Config`'s borsh form, which stays out of the public type.
fn write_config<W: io::Write>(c: &Config, w: &mut W) -> io::Result<()> {
    let Config {
        protocol_version,
        chain_id,
        gas_price,
        block_gas_limit,
        block_time,
        seed,
        epoch_length,
        epoch_reward,
        minimum_stake,
        max_blocks,
        gas_limits,
        gas_pricing,
    } = c;
    let block_time = (block_time.as_secs(), block_time.subsec_nanos());
    let epochs = (epoch_length, epoch_reward, minimum_stake);
    let limits = match gas_limits {
        GasLimits::Protocol => (0u8, Gas::ZERO, Gas::ZERO),
        GasLimits::Legacy300 => (1, Gas::ZERO, Gas::ZERO),
        GasLimits::Custom { prepaid, burnt } => (2, *prepaid, *burnt),
    };
    let rest = (seed, epochs, max_blocks, limits, gas_pricing);
    (protocol_version, chain_id, gas_price, block_gas_limit, block_time, rest).serialize(w)
}

fn read_config<R: io::Read>(r: &mut R) -> io::Result<Config> {
    let (
        protocol_version,
        chain_id,
        gas_price,
        block_gas_limit,
        (secs, nanos),
        (seed, epochs, max_blocks, limits, gas_pricing),
    ) = BorshDeserialize::deserialize_reader(r)?;
    let (epoch_length, epoch_reward, minimum_stake) = epochs;
    let invalid = |why: &str| io::Error::new(io::ErrorKind::InvalidData, why.to_owned());
    let gas_limits = match limits {
        (0u8, _, _) => GasLimits::Protocol,
        (1, _, _) => GasLimits::Legacy300,
        (2, prepaid, burnt) => GasLimits::Custom { prepaid, burnt },
        _ => return Err(invalid("invalid gas_limits")),
    };
    if nanos >= 1_000_000_000 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid block_time nanoseconds"));
    }
    let block_time = Duration::new(secs, nanos);
    Ok(Config {
        protocol_version,
        chain_id,
        gas_price,
        block_gas_limit,
        block_time,
        seed,
        epoch_length,
        epoch_reward,
        minimum_stake,
        max_blocks,
        gas_limits,
        gas_pricing,
    })
}

impl Sim {
    /// Runs `setup` once per build of the test binary and saves its chain to
    /// disk; later calls from the same place with the same `key`, in this
    /// test process or another, load it instead. Concurrent misses wait for
    /// one `setup`.
    ///
    /// Put in `key` what `setup` reads that the test binary does not contain,
    /// typically the wasm it deploys (`NEAR_SIM_GAS_LIMITS` is keyed
    /// already). Rebuilding the binary (any change to the
    /// setup code, this crate, nearcore or the toolchain) starts afresh, and two
    /// call sites never share a snapshot, even with the same key. A load has
    /// the same state root, clock, config, block log (every outcome and the
    /// conservation ledger), keys and signed delegates' nonces, so it goes on
    /// exactly like the original. Only the `Sim` is kept: get accounts back by
    /// id and keys with `secret_key(id)`. Panics if `setup` leaves anything
    /// queued or in flight (`settle()` settles it); parked yields are fine.
    ///
    /// ```no_run
    /// # use near_contract_sim::{Balance, Sim};
    /// let wasm = std::fs::read("tests/res/auction.wasm").expect("build tests/res/auction.wasm first");
    /// let mut sim = Sim::cached(&wasm, || {
    ///     let mut sim = Sim::new();
    ///     sim.create_account("alice.near", Balance::from_near(100));
    ///     sim.deploy("app.near", wasm.clone());
    ///     sim
    /// });
    /// ```
    #[track_caller]
    pub fn cached(key: impl Hash, setup: impl FnOnce() -> Sim) -> Sim {
        let exe = std::env::current_exe().expect("test executable");
        let meta = exe.metadata().expect("test executable");
        // The default limits come from the environment
        // (`NEAR_SIM_GAS_LIMITS`): a setup run under one keeps them.
        let site = hash_of((key, Location::caller(), Config::default().gas_limits));
        let dir = cache_dir().join("snapshots").join(exe.file_name().expect("executable name"));
        cached_at(
            &dir.join(format!("{site:016x}")),
            hash_of((site, meta.len(), meta.modified().ok())),
            setup,
        )
    }

    /// An overdue yield timeout is no obstacle: it is trie state, and the
    /// loaded chain finds it overdue too.
    #[track_caller]
    fn snapshot(&self) -> Snapshot {
        assert!(
            !self.chain.is_holding(),
            "a snapshot needs no receipt held (Fault::Hold): settle() first, which releases them"
        );
        assert!(
            self.chain.is_settled(),
            "a snapshot needs nothing queued or in flight (settle() first); parked yields are fine"
        );
        Snapshot {
            entries: self.chain.entries(),
            root: self.chain.state_root(),
            config: self.config.clone(),
            height: self.chain.height,
            timestamp_ns: self.chain.timestamp_ns,
            log: self.chain.log().clone(),
            keys: self.keys.iter().map(|(id, sk)| (id.clone(), sk.to_string())).collect(),
            delegates: self.delegates.clone(),
            paused: self.paused.map(|d| (d.as_secs(), d.subsec_nanos())),
            pinned: (self.pinned.height, self.pinned.time),
            viewed: self.viewed.lock().map(|v| v.iter().cloned().collect()).unwrap_or_default(),
        }
    }

    /// `None` if the rebuilt trie doesn't have the snapshot's root.
    fn from_snapshot(s: Snapshot) -> Option<Sim> {
        let mut chain = Chain::new(s.height, s.timestamp_ns);
        chain.load(s.entries, s.root, s.log)?;
        let keys = s.keys.into_iter().map(|(id, sk)| Some((id, sk.parse().ok()?)));
        let keys = keys.collect::<Option<_>>()?;
        let paused = match s.paused {
            Some((secs, nanos)) if nanos < 1_000_000_000 => Some(Duration::new(secs, nanos)),
            Some(_) => return None,
            None => None,
        };
        Some(Sim {
            config: s.config,
            chain,
            keys,
            delegates: s.delegates,
            paused,
            pinned: crate::sim::Pinned { height: s.pinned.0, time: s.pinned.1 },
            explore: None,
            viewed: std::sync::Arc::new(std::sync::Mutex::new(s.viewed.into_iter().collect())),
        })
    }
}

/// Loads the snapshot at `path` if it was
/// saved under `key`. Otherwise (no file, another key, a corrupt file) runs
/// `setup` and saves its result there. Concurrent misses wait for one
/// `setup` under a file lock, and the file is replaced by a rename, so a
/// reader never sees a partial one.
#[track_caller]
fn cached_at(path: &Path, key: u64, setup: impl FnOnce() -> Sim) -> Sim {
    let load = || {
        let bytes = std::fs::read(path).ok()?;
        let (k, s) = borsh::from_slice::<(u64, Snapshot)>(&bytes).ok()?;
        if k == key { Sim::from_snapshot(s) } else { None }
    };
    if let Some(hit) = load() {
        return hit;
    }
    std::fs::create_dir_all(path.parent().expect("snapshot directory"))
        .expect("snapshot directory");
    let lock = File::create(path.with_extension("lock")).expect("snapshot lock file");
    lock.lock().expect("snapshot lock");
    if let Some(hit) = load() {
        return hit;
    }
    let sim = setup();
    let tmp = path.with_extension("tmp");
    let bytes = borsh::to_vec(&(key, sim.snapshot())).expect("snapshot");
    if let Err(e) = std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, path)) {
        panic!("writing snapshot {}: {e}", path.display());
    }
    sim
}

/// The same in every process (`DefaultHasher::new`, not `RandomState`).
pub(crate) fn hash_of(v: impl Hash) -> u64 {
    let mut h = DefaultHasher::new();
    v.hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Balance;
    use std::cell::Cell;

    fn with_users<'a>(ids: impl IntoIterator<Item = &'a str>) -> Sim {
        let mut sim = Sim::new();
        for id in ids {
            sim.create_account(id, Balance::from_near(1_000));
        }
        sim
    }

    #[test]
    fn a_missing_stale_or_corrupt_snapshot_is_a_miss() {
        let path = cache_dir().join(format!("snapshot-test-{}", std::process::id()));
        let runs = Cell::new(0);
        let setup = || {
            runs.set(runs.get() + 1);
            with_users(["alice.near"])
        };
        let root = cached_at(&path, 1, setup).state_root();
        assert_eq!(cached_at(&path, 1, setup).state_root(), root);
        assert_eq!(runs.get(), 1);
        cached_at(&path, 2, setup);
        assert_eq!(runs.get(), 2);
        let bytes = std::fs::read(&path).unwrap();
        std::fs::write(&path, &bytes[..bytes.len() - 1]).unwrap();
        cached_at(&path, 2, setup);
        cached_at(&path, 2, setup);
        assert_eq!(runs.get(), 3);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn corrupt_config_duration_is_an_error_not_a_panic() {
        let c = Config {
            minimum_stake: near_primitives::types::Balance::from_near(7),
            ..Config::default()
        };
        let encode = |nanos: u32| {
            borsh::to_vec(&(
                c.protocol_version,
                &c.chain_id,
                c.gas_price,
                c.block_gas_limit,
                (u64::MAX, nanos),
                (
                    c.seed,
                    (c.epoch_length, c.epoch_reward, c.minimum_stake),
                    c.max_blocks,
                    (0u8, Gas::ZERO, Gas::ZERO),
                    Some(81u32),
                ),
            ))
            .unwrap()
        };
        let valid = read_config(&mut &encode(999_999_999)[..]).unwrap();
        assert_eq!(valid.block_time, Duration::new(u64::MAX, 999_999_999));
        assert_eq!(valid.gas_pricing, Some(81));
        assert_eq!(valid.minimum_stake, near_primitives::types::Balance::from_near(7));
        for nanos in [1_000_000_000, u32::MAX] {
            assert_eq!(
                read_config(&mut &encode(nanos)[..]).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
    }

    /// Two chains built alike save to the same bytes.
    #[test]
    fn snapshots_are_reproducible() {
        let users: Vec<String> = (0..8).map(|i| format!("u{i}.near")).collect();
        let save = || borsh::to_vec(&with_users(users.iter().map(String::as_str)).snapshot());
        assert_eq!(save().unwrap(), save().unwrap());
    }

    #[test]
    fn a_snapshot_whose_entries_miss_its_root_is_refused() {
        let mut s = with_users(["alice.near"]).snapshot();
        s.root = CryptoHash::default();
        assert!(Sim::from_snapshot(s).is_none());
    }

    #[test]
    #[should_panic(expected = "a snapshot needs nothing queued or in flight")]
    fn only_a_settled_chain_is_saved() {
        let mut sim = with_users(["alice.near"]);
        sim.call("alice.near", "alice.near", "m").submit();
        sim.snapshot();
    }
}
