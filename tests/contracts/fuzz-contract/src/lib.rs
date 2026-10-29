//! Programmable fuzz contract for near-contract-sim's property and
//! differential tests.
//!
//! One entry point, `run`, whose input is a borsh-encoded `Vec<Op>`. Every
//! op either touches storage, reads the environment, logs, or builds
//! promises; results are logged in a fixed-width format so that the wasm
//! instruction count does not depend on the *values* of chain-specific data
//! (heights, timestamps, ids): e.g. u64s are always printed as 16 hex digits
//! by a branch-free loop. That keeps gas bit-identical between the simulator
//! and a real node even though heights and ids differ.
//!
//! Nested programs (call targets, callbacks, yield callbacks) are carried as
//! already-encoded bytes and passed verbatim as the `run` arguments.
//!
//! `digest` (view) returns borsh `Vec<Option<Vec<u8>>>` for keys `k0`..`k7`.
//!
//! The op encoding is mirrored in `tests/fuzz_common/mod.rs` (the generator).

use near_sdk::borsh::{self, BorshDeserialize};
use near_sdk::sys;

#[derive(BorshDeserialize)]
#[borsh(crate = "near_sdk::borsh")]
pub enum Op {
    /// 0: storage_write; logs `w<evicted>`
    Write { key: Vec<u8>, value: Vec<u8> },
    /// 1: storage_read; logs `r<hex value>` or `r-`
    Read { key: Vec<u8> },
    /// 2: storage_remove; logs `d<hex old value>` or `d-`
    Remove { key: Vec<u8> },
    /// 3: log_utf8
    Log(String),
    /// 4: log an environment value (see `env_value`)
    Env(u8),
    /// 5: value_return
    Return(Vec<u8>),
    /// 6: panic_utf8("fuzz panic")
    Panic,
    /// 7: promise_batch_create(to) + function_call_weight("run", args); pushes
    Call { to: String, args: Vec<u8>, deposit: u128, gas: u64, weight: u64 },
    /// 8: promise_batch_create(to) + transfer; pushes
    Transfer { to: String, amount: u128 },
    /// 9: pops p, promise_batch_then(p, current) + function_call_weight("run", args); pushes
    Then { args: Vec<u8>, gas: u64, weight: u64 },
    /// 10: pops two, promise_and; pushes
    And,
    /// 11: pops p, promise_return(p)
    PromiseReturn,
    /// 12: logs promise_results_count and every result
    Results,
    /// 13: promise_yield_create("run", args); stores the data id at `Y<slot>`; pushes
    YieldCreate { slot: u8, args: Vec<u8>, gas: u64, weight: u64 },
    /// 14: promise_yield_resume(storage[`Y<slot>`], payload); logs `y<ok>`
    YieldResume { slot: u8, payload: Vec<u8> },
    /// 15: promise_yield_create_with_id("run", args, deposit, id=[id;32]); pushes (or logs `Y!` on duplicate)
    YieldCreateWithId { id: u8, args: Vec<u8>, deposit: u128, gas: u64, weight: u64 },
    /// 16: promise_yield_resume_with_yield_id([id;32], payload); logs `z<ok>`
    YieldResumeWithId { id: u8, payload: Vec<u8> },
    /// 17: promise_batch_create(<name>.<current>) + create_account + transfer; pushes
    CreateAccount { name: String, amount: u128 },
    /// 18: burn gas with n sha256 rounds
    BurnGas(u32),
}

const HEX: &[u8; 16] = b"0123456789abcdef";
const REG: u64 = 0;

fn hex_into(out: &mut Vec<u8>, bytes: &[u8]) {
    for b in bytes {
        out.push(HEX[(b >> 4) as usize]);
        out.push(HEX[(b & 15) as usize]);
    }
}

fn log(bytes: &[u8]) {
    unsafe { sys::log_utf8(bytes.len() as u64, bytes.as_ptr() as u64) }
}

fn register() -> Vec<u8> {
    unsafe {
        let len = sys::register_len(REG);
        let mut v = vec![0u8; len as usize];
        sys::read_register(REG, v.as_mut_ptr() as u64);
        v
    }
}

fn u128_at(f: unsafe extern "C" fn(u64)) -> u128 {
    let mut buf = [0u8; 16];
    unsafe { f(buf.as_mut_ptr() as u64) };
    u128::from_le_bytes(buf)
}

/// Fixed-width encoding of an environment value.
fn env_value(k: u8, out: &mut Vec<u8>) {
    unsafe {
        match k {
            0 => hex_into(out, &sys::block_timestamp().to_be_bytes()),
            1 => hex_into(out, &sys::block_index().to_be_bytes()),
            2 => hex_into(out, &sys::epoch_height().to_be_bytes()),
            3 => {
                sys::random_seed(REG);
                hex_into(out, &register())
            }
            4 => {
                sys::predecessor_account_id(REG);
                out.extend(register())
            }
            5 => {
                sys::signer_account_id(REG);
                out.extend(register())
            }
            6 => {
                sys::current_account_id(REG);
                out.extend(register())
            }
            7 => hex_into(out, &u128_at(sys::attached_deposit).to_be_bytes()),
            8 => hex_into(out, &sys::prepaid_gas().to_be_bytes()),
            9 => hex_into(out, &sys::used_gas().to_be_bytes()),
            10 => hex_into(out, &u128_at(sys::account_balance).to_be_bytes()),
            11 => hex_into(out, &sys::storage_usage().to_be_bytes()),
            12 => hex_into(out, &u128_at(sys::account_locked_balance).to_be_bytes()),
            _ => out.push(b'?'),
        }
    }
}

fn current_account() -> Vec<u8> {
    unsafe { sys::current_account_id(REG) };
    register()
}

fn fc(p: u64, args: &[u8], deposit: u128, gas: u64, weight: u64) {
    let m = b"run";
    unsafe {
        sys::promise_batch_action_function_call_weight(
            p,
            m.len() as u64,
            m.as_ptr() as u64,
            args.len() as u64,
            args.as_ptr() as u64,
            &deposit as *const u128 as u64,
            gas,
            weight,
        )
    }
}

fn exec(ops: Vec<Op>) {
    let mut stack: Vec<u64> = Vec::new();
    for op in ops {
        let mut line: Vec<u8> = Vec::with_capacity(64);
        match op {
            Op::Write { key, value } => unsafe {
                let ev = sys::storage_write(
                    key.len() as u64,
                    key.as_ptr() as u64,
                    value.len() as u64,
                    value.as_ptr() as u64,
                    REG,
                );
                line.push(b'w');
                line.push(b'0' + ev as u8);
            },
            Op::Read { key } => unsafe {
                line.push(b'r');
                if sys::storage_read(key.len() as u64, key.as_ptr() as u64, REG) == 1 {
                    hex_into(&mut line, &register());
                } else {
                    line.push(b'-');
                }
            },
            Op::Remove { key } => unsafe {
                line.push(b'd');
                if sys::storage_remove(key.len() as u64, key.as_ptr() as u64, REG) == 1 {
                    hex_into(&mut line, &register());
                } else {
                    line.push(b'-');
                }
            },
            Op::Log(s) => line.extend(s.as_bytes()),
            Op::Env(k) => {
                line.push(b'e');
                line.push(HEX[(k & 15) as usize]);
                line.push(b':');
                env_value(k, &mut line);
            }
            Op::Return(v) => unsafe { sys::value_return(v.len() as u64, v.as_ptr() as u64) },
            Op::Panic => {
                let m = b"fuzz panic";
                unsafe { sys::panic_utf8(m.len() as u64, m.as_ptr() as u64) }
            }
            Op::Call { to, args, deposit, gas, weight } => {
                let p = unsafe { sys::promise_batch_create(to.len() as u64, to.as_ptr() as u64) };
                fc(p, &args, deposit, gas, weight);
                stack.push(p);
            }
            Op::Transfer { to, amount } => unsafe {
                let p = sys::promise_batch_create(to.len() as u64, to.as_ptr() as u64);
                sys::promise_batch_action_transfer(p, &amount as *const u128 as u64);
                stack.push(p);
            },
            Op::Then { args, gas, weight } => match stack.pop() {
                Some(prev) => {
                    let me = current_account();
                    let p = unsafe { sys::promise_batch_then(prev, me.len() as u64, me.as_ptr() as u64) };
                    fc(p, &args, 0, gas, weight);
                    stack.push(p);
                }
                None => line.extend(b"t!"),
            },
            Op::And => {
                if stack.len() >= 2 {
                    let b = stack.pop().unwrap();
                    let a = stack.pop().unwrap();
                    let ids = [a, b];
                    stack.push(unsafe { sys::promise_and(ids.as_ptr() as u64, 2) });
                } else {
                    line.extend(b"a!");
                }
            }
            Op::PromiseReturn => match stack.pop() {
                Some(p) => unsafe { sys::promise_return(p) },
                None => line.extend(b"p!"),
            },
            Op::Results => unsafe {
                let n = sys::promise_results_count();
                line.push(b'n');
                line.push(b'0' + n as u8);
                for i in 0..n {
                    line.push(b'|');
                    match sys::promise_result(i, REG) {
                        1 => {
                            line.push(b'+');
                            hex_into(&mut line, &register());
                        }
                        2 => line.push(b'x'),
                        _ => line.push(b'?'),
                    }
                }
            },
            Op::YieldCreate { slot, args, gas, weight } => unsafe {
                let m = b"run";
                let p = sys::promise_yield_create(
                    m.len() as u64,
                    m.as_ptr() as u64,
                    args.len() as u64,
                    args.as_ptr() as u64,
                    gas,
                    weight,
                    REG,
                );
                let key = [b'Y', slot];
                sys::storage_write(2, key.as_ptr() as u64, u64::MAX, REG, 1);
                stack.push(p);
            },
            Op::YieldResume { slot, payload } => unsafe {
                let key = [b'Y', slot];
                line.push(b'y');
                if sys::storage_read(2, key.as_ptr() as u64, REG) == 1 {
                    let ok = sys::promise_yield_resume(
                        u64::MAX,
                        REG,
                        payload.len() as u64,
                        payload.as_ptr() as u64,
                    );
                    line.push(b'0' + ok as u8);
                } else {
                    line.push(b'-');
                }
            },
            Op::YieldCreateWithId { id, args, deposit, gas, weight } => unsafe {
                let m = b"run";
                let yid = [id; 32];
                let p = sys::promise_yield_create_with_id(
                    m.len() as u64,
                    m.as_ptr() as u64,
                    args.len() as u64,
                    args.as_ptr() as u64,
                    &deposit as *const u128 as u64,
                    gas,
                    weight,
                    32,
                    yid.as_ptr() as u64,
                );
                if p == u64::MAX {
                    line.extend(b"Y!");
                } else {
                    stack.push(p);
                }
            },
            Op::YieldResumeWithId { id, payload } => unsafe {
                let yid = [id; 32];
                let ok = sys::promise_yield_resume_with_yield_id(
                    32,
                    yid.as_ptr() as u64,
                    payload.len() as u64,
                    payload.as_ptr() as u64,
                );
                line.push(b'z');
                line.push(b'0' + ok as u8);
            },
            Op::CreateAccount { name, amount } => unsafe {
                let mut id = name.into_bytes();
                id.push(b'.');
                id.extend(current_account());
                let p = sys::promise_batch_create(id.len() as u64, id.as_ptr() as u64);
                sys::promise_batch_action_create_account(p);
                sys::promise_batch_action_transfer(p, &amount as *const u128 as u64);
                stack.push(p);
            },
            Op::BurnGas(n) => unsafe {
                let mut buf = [0u8; 32];
                for _ in 0..n {
                    sys::sha256(32, buf.as_ptr() as u64, REG);
                    sys::read_register(REG, buf.as_mut_ptr() as u64);
                }
            },
        }
        if !line.is_empty() {
            log(&line);
        }
    }
}

#[no_mangle]
pub extern "C" fn run() {
    unsafe { sys::input(REG) };
    let input = register();
    match Vec::<Op>::try_from_slice(&input) {
        Ok(ops) => exec(ops),
        Err(_) => {
            let m = b"bad program";
            unsafe { sys::panic_utf8(m.len() as u64, m.as_ptr() as u64) }
        }
    }
}

/// View: borsh `Vec<Option<Vec<u8>>>` of keys `k0`..`k7`.
#[no_mangle]
pub extern "C" fn digest() {
    let mut out: Vec<Option<Vec<u8>>> = Vec::with_capacity(8);
    for i in 0..8u8 {
        let key = [b'k', b'0' + i];
        let v = unsafe {
            if sys::storage_read(2, key.as_ptr() as u64, REG) == 1 { Some(register()) } else { None }
        };
        out.push(v);
    }
    let bytes = borsh::to_vec(&out).unwrap();
    unsafe { sys::value_return(bytes.len() as u64, bytes.as_ptr() as u64) }
}
