//! Which functions an `unreachable` trap is in. A Rust contract built with
//! `panic = "abort"` traps on every failed `unwrap`, overflow or `require!`
//! with the same message, from the standard library's abort.
//! [`instrument`] copies a module so that each function outside the
//! standard library keeps the innermost [`FRAMES`] such functions running
//! in new globals (set on entry and again after each call it makes, from
//! locals), and each `unreachable` first logs them; [`name`] reads the log
//! back through the module's name section. The copy costs more gas, so a
//! run on it diagnoses and never judges.

use std::collections::HashMap;
use wasmparser::{Name, Operator, Parser, Payload, TypeRef};

/// What the copy logs before it traps: this, then each frame's function
/// index as eight letters, a nibble each (`a` is 0), innermost first.
const MARK: &str = "trap@";

/// The frames a trap names: the function and its callers.
const FRAMES: u32 = 8;

/// nearcore's message for the trap.
pub(crate) const UNREACHABLE: &str = "An `unreachable` opcode was executed.";

/// `msg` naming the functions (`name`'s): `... was executed in `f`, called
/// from `g`.`
pub(crate) fn named(msg: &str, frames: &str) -> String {
    msg.replace(UNREACHABLE, &format!("An `unreachable` opcode was executed in {frames}."))
}

/// A function of the standard library, which a trap is never blamed on.
fn library(name: &str) -> bool {
    let name = name.trim_start_matches('<');
    ["core::", "std::", "alloc::", "compiler_builtins::", "dlmalloc::", "__rust"]
        .iter()
        .any(|p| name.starts_with(p))
}

/// The functions `wasm` exports, in name order.
pub(crate) fn exports(wasm: &[u8]) -> Vec<String> {
    let mut out: Vec<String> = Parser::new(0)
        .parse_all(wasm)
        .flatten()
        .filter_map(|p| match p {
            Payload::ExportSection(s) => Some(s),
            _ => None,
        })
        .flat_map(|s| s.into_iter().flatten())
        .filter(|e| e.kind == wasmparser::ExternalKind::Func)
        .map(|e| e.name.to_string())
        .collect();
    out.sort();
    out
}

/// The module's function names, by index, demangled, without Rust's hash.
fn names(wasm: &[u8]) -> HashMap<u32, String> {
    let mut out = HashMap::new();
    for payload in Parser::new(0).parse_all(wasm).flatten() {
        let Payload::CustomSection(c) = payload else { continue };
        let wasmparser::KnownCustom::Name(reader) = c.as_known() else { continue };
        for name in reader.into_iter().flatten() {
            let Name::Function(map) = name else { continue };
            for n in map.into_iter().flatten() {
                out.insert(n.index, format!("{:#}", rustc_demangle::demangle(n.name)));
            }
        }
    }
    out
}

/// The functions `log` (a line the copy of `wasm` logged) names:
/// `` `f`, called from `g`, from `h` ``.
pub(crate) fn name(wasm: &[u8], log: &str) -> Option<String> {
    let digits = log.strip_prefix(MARK)?.as_bytes();
    let names = names(wasm);
    let frames: Vec<String> = (digits.chunks(8))
        .map(|d| d.iter().fold(0u32, |n, d| (n << 4) | u32::from(d.wrapping_sub(b'a') & 15)))
        .map_while(|i| Some(format!("`{}`", names.get(&i)?)))
        .collect();
    let (first, callers) = frames.split_first()?;
    Some(match callers {
        [] => first.clone(),
        _ => format!("{first}, called from {}", callers.join(", from ")),
    })
}

/// The copy of `wasm` described above, or `None` for a module without
/// function names, its own 32-bit memory or a `log_utf8` import to log
/// with.
pub(crate) fn instrument(wasm: &[u8]) -> Option<Vec<u8>> {
    let names = names(wasm);
    let (mut params, mut funcs, mut globals, mut log, mut memory) = (vec![], 0, 0, None, false);
    let (mut types, mut bodies) = (vec![], vec![]);
    for payload in Parser::new(0).parse_all(wasm) {
        match payload.ok()? {
            Payload::TypeSection(s) => {
                for t in s.into_iter_err_on_gc_types() {
                    params.push(t.ok()?.params().len() as u32);
                }
            }
            Payload::ImportSection(s) => {
                for i in s {
                    let i = i.ok()?;
                    match i.ty {
                        TypeRef::Func(_) if i.name == "log_utf8" => log = Some(funcs),
                        TypeRef::Global(_) => globals += 1,
                        TypeRef::Memory(m) => memory = !m.memory64,
                        _ => {}
                    }
                    funcs += u32::from(matches!(i.ty, TypeRef::Func(_)));
                }
            }
            Payload::FunctionSection(s) => types = s.into_iter().collect::<Result<_, _>>().ok()?,
            Payload::MemorySection(s) => memory = s.into_iter().flatten().any(|m| !m.memory64),
            Payload::GlobalSection(s) => globals += s.count(),
            Payload::CodeSectionEntry(body) => bodies.push(body),
            _ => {}
        }
    }
    if names.is_empty() || !memory || bodies.is_empty() {
        return None;
    }
    let (log, report) = (log?, funcs + bodies.len() as u32);
    let mut code = vec![];
    for (k, body) in bodies.iter().enumerate() {
        let index = funcs + k as u32;
        let own = names.get(&index).is_some_and(|n| !library(n));
        let range = body.range();
        let mut ops = body.get_operators_reader().ok()?;
        let start = ops.original_position();
        let (mut out, mut set) = (vec![], vec![]);
        if own {
            // The frames its callers were in, kept in new locals from `base`.
            let (groups, used) = read_uleb(&wasm[range.start..])?;
            let locals = body.get_locals_reader().ok()?.into_iter();
            let declared: u32 = locals.map(|l| l.map(|l| l.0)).sum::<Result<_, _>>().ok()?;
            let base = params.get(*types.get(k)? as usize)? + declared;
            out.extend(uleb(groups + 1));
            out.extend(&wasm[range.start + used..start]);
            out.extend(uleb(FRAMES - 1));
            out.push(0x7f);
            for j in 0..FRAMES - 1 {
                out.extend([&[0x23][..], &uleb(globals + j), &[0x21], &uleb(base + j)].concat());
            }
            for j in 1..FRAMES {
                set.extend(
                    [&[0x20][..], &uleb(base + j - 1), &[0x24], &uleb(globals + j)].concat(),
                );
            }
            set.extend(
                [&[0x41][..], &sleb(i64::from(index as i32)), &[0x24], &uleb(globals)].concat(),
            );
            out.extend(&set);
        } else {
            out.extend(&wasm[range.start..start]);
        }
        let mut from = start;
        while !ops.eof() {
            let (op, at) = ops.read_with_offset().ok()?;
            match op {
                Operator::Unreachable => {
                    out.extend(&wasm[from..at]);
                    out.extend([&[0x10][..], &uleb(report)].concat());
                    from = at;
                }
                Operator::Call { .. } | Operator::CallIndirect { .. } if own => {
                    let end = ops.original_position();
                    out.extend(&wasm[from..end]);
                    out.extend(&set);
                    from = end;
                }
                _ => {}
            }
        }
        out.extend(&wasm[from..range.end]);
        code.push(out);
    }
    code.push(report_body(globals, log));
    rebuild(wasm, params.len() as u32, &code)
}

/// `report`: writes the mark and each frame's nibbles at address 0, then
/// logs them.
fn report_body(globals: u32, log: u32) -> Vec<u8> {
    let mut b = vec![0];
    let mut store = |at: usize, value: &[u8]| {
        b.extend([&[0x41][..], &sleb(at as i64), value, &[0x3a, 0, 0]].concat());
    };
    for (at, byte) in MARK.bytes().enumerate() {
        store(at, &[&[0x41][..], &sleb(byte.into())].concat());
    }
    for j in 0..FRAMES {
        for k in 0..8 {
            let shift = sleb(28 - 4 * k as i64);
            // (global >> shift & 15) + 'a'
            let letter =
                [&[0x23][..], &uleb(globals + j), &[0x41], &shift, &[0x76, 0x41, 15, 0x71]];
            let letter = [&letter.concat()[..], &[0x41], &sleb(97), &[0x6a]].concat();
            store(MARK.len() + (8 * j + k) as usize, &letter);
        }
    }
    b.push(0x42);
    b.extend(sleb((MARK.len() + 8 * FRAMES as usize) as i64));
    b.extend([0x42, 0, 0x10]);
    b.extend(uleb(log));
    b.push(0x0b);
    b
}

/// `wasm` with a `() -> ()` type, the frames' globals and the code bodies
/// `code` (the last one new), sections otherwise as they were.
fn rebuild(wasm: &[u8], types: u32, code: &[Vec<u8>]) -> Option<Vec<u8>> {
    let mut out = wasm.get(..8)?.to_vec();
    let section = |out: &mut Vec<u8>, id: u8, content: &[u8]| {
        out.push(id);
        out.extend(uleb(content.len() as u32));
        out.extend(content);
    };
    // A count, then entries; `more` appended after them.
    let append = |content: &[u8], n: u32, more: &[u8]| -> Option<Vec<u8>> {
        let (count, used) = read_uleb(content)?;
        Some([&uleb(count + n)[..], &content[used..], more].concat())
    };
    // Mutable i32s from -1, which names no function.
    let frames: Vec<u8> = (0..FRAMES).flat_map(|_| [0x7f, 0x01, 0x41, 0x7f, 0x0b]).collect();
    let (mut i, mut placed) = (8, false);
    while i < wasm.len() {
        let id = wasm[i];
        let (len, used) = read_uleb(&wasm[i + 1..])?;
        let content = wasm.get(i + 1 + used..i + 1 + used + len as usize)?;
        i += 1 + used + len as usize;
        // The global section (6) goes after memory (5) and tags (13).
        if !placed && ![0, 1, 2, 3, 4, 5, 6, 13].contains(&id) {
            section(&mut out, 6, &[&uleb(FRAMES)[..], &frames].concat());
            placed = true;
        }
        match id {
            1 => section(&mut out, 1, &append(content, 1, &[0x60, 0, 0])?),
            3 => section(&mut out, 3, &append(content, 1, &uleb(types))?),
            6 => {
                section(&mut out, 6, &append(content, FRAMES, &frames)?);
                placed = true;
            }
            10 => {
                let bodies: Vec<u8> =
                    code.iter().flat_map(|b| [uleb(b.len() as u32), b.clone()].concat()).collect();
                section(&mut out, 10, &[&uleb(code.len() as u32)[..], &bodies].concat());
            }
            _ => section(&mut out, id, content),
        }
    }
    Some(out)
}

fn uleb(mut n: u32) -> Vec<u8> {
    let mut out = vec![];
    loop {
        let byte = (n & 0x7f) as u8;
        n >>= 7;
        if n == 0 {
            out.push(byte);
            return out;
        }
        out.push(byte | 0x80);
    }
}

fn sleb(mut n: i64) -> Vec<u8> {
    let mut out = vec![];
    loop {
        let byte = (n & 0x7f) as u8;
        n >>= 7;
        if (n == 0 && byte & 0x40 == 0) || (n == -1 && byte & 0x40 != 0) {
            out.push(byte);
            return out;
        }
        out.push(byte | 0x80);
    }
}

/// A u32 LEB128 at the start of `b`, and its length.
fn read_uleb(b: &[u8]) -> Option<(u32, usize)> {
    let (mut n, mut shift) = (0u32, 0);
    for (i, &byte) in b.iter().enumerate().take(5) {
        n |= u32::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some((n, i + 1));
        }
        shift += 7;
    }
    None
}
