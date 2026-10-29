//! Strategies for an op's fields that favor the edges where size bugs
//! hide (a fixed callback gas meeting growing work, a log past 16 KiB, a
//! key past 2 KiB), which a uniform size almost never lands on. About half
//! the values are edges (empty, one, a few, the host's limits and one
//! either side, the maximum), the rest log-uniform. Lengths shrink toward
//! short, lists toward fewer then simpler items; content doesn't shrink.
//!
//! ```
//! use near_contract_sim::explore::input;
//! use proptest::prelude::*;
//! use proptest::strategy::ValueTree;
//! use proptest::test_runner::TestRunner;
//! let mut runner = TestRunner::deterministic();
//! let ids: Vec<String> = input::list(input::id()).new_tree(&mut runner).unwrap().current();
//! assert!(ids.len() <= 64 && ids.iter().all(|id| id.len() <= 64));
//! let memo = input::text().max(100).new_tree(&mut runner).unwrap().current();
//! assert!(memo.len() <= 100);
//! ```

use proptest::num::usize::BinarySearch;
use proptest::prelude::*;
use proptest::strategy::{NewTree, ValueTree};
use proptest::test_runner::TestRunner;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Host lines a string often meets: the longest AccountId, the longest
/// storage key, the per-receipt log limit (`max_total_log_length`).
pub(crate) const HOST_SIZES: [usize; 3] = [64, 2048, 16384];

const ID_MAX: usize = 64;
/// One byte past the log limit.
const TEXT_MAX: usize = 16385;
const LIST_MAX: usize = 64;
/// [`Shape::Huge`]'s size: past every host size, within the argument and
/// return limits.
pub(crate) const HUGE: usize = 65536;
const EDGE_SHARE: f64 = 0.5;

/// A size from `min` to `max`: an edge in about half the draws, else
/// log-uniform.
#[derive(Clone, Copy, Debug)]
struct Size {
    min: usize,
    max: usize,
}

#[derive(Clone, Copy)]
enum Edge {
    At(usize),
    /// 2 to 4 more than the minimum.
    Few,
}

impl Size {
    fn edges(self) -> Vec<Edge> {
        let (lo, hi) = (self.min, self.max);
        let mut at = vec![lo, lo + 1];
        for h in HOST_SIZES {
            at.extend([h - 1, h, h + 1]);
        }
        at.extend([hi.saturating_sub(1), hi]);
        at.retain(|n| (lo..=hi).contains(n));
        at.sort_unstable();
        at.dedup();
        let mut edges: Vec<Edge> = at.into_iter().map(Edge::At).collect();
        if hi >= lo + 4 {
            edges.push(Edge::Few);
        }
        edges
    }

    fn draw(self, runner: &mut TestRunner) -> Result<usize, proptest::test_runner::Reason> {
        let edge = prop::bool::weighted(EDGE_SHARE).new_tree(runner)?.current();
        if edge {
            let edges = self.edges();
            let k = (0..edges.len()).new_tree(runner)?.current();
            return Ok(match edges[k] {
                Edge::At(n) => n,
                Edge::Few => self.min + (2..=4usize).new_tree(runner)?.current(),
            });
        }
        // (span + 1)^u for u in [0, 1): from 1 above the minimum, an edge
        // already, which would otherwise come up in a sixth of id draws.
        let u = (0.0..1.0f64).new_tree(runner)?.current();
        let span = self.max - self.min;
        let n = ((span as f64 + 1.0).powf(u)).floor() as usize;
        Ok(self.min + n.clamp(span.min(1), span))
    }
}

/// Strings of `[a-z0-9]` whose lengths favor the edges.
#[derive(Clone, Copy, Debug)]
#[must_use]
pub struct Text {
    size: Size,
    /// Draws an escape-heavy family too ([`text`], not [`id`]).
    escapes: bool,
}

/// A name, key or account id: 0 to 64 bytes of `[a-z0-9]`, a valid
/// AccountId from 2 bytes up.
pub fn id() -> Text {
    Text { size: Size { min: 0, max: ID_MAX }, escapes: false }
}

/// Free text, such as a memo: 0 to 16 KiB + 1 bytes, one past the log
/// limit (not 4 MiB: `max_transaction_size` is 1.5 MiB). In 1 draw of 8 it
/// is escape-heavy, which JSON escaping grows up to 6 times: a contract
/// parsing it pays for the escaped size.
pub fn text() -> Text {
    Text { size: Size { min: 0, max: TEXT_MAX }, escapes: true }
}

impl Text {
    /// At most `max` bytes: a field limit to probe past.
    pub fn max(mut self, max: usize) -> Self {
        self.size.max = max;
        self.size.min = self.size.min.min(max);
        self
    }

    /// At least `min` bytes.
    #[track_caller]
    pub fn min(mut self, min: usize) -> Self {
        assert!(min <= self.size.max, "input: min {min} above max {}", self.size.max);
        self.size.min = min;
        self
    }
}

impl Strategy for Text {
    type Tree = TextTree;
    type Value = String;

    fn new_tree(&self, runner: &mut TestRunner) -> NewTree<Self> {
        let len = self.size.draw(runner)?;
        let seed = any::<u64>().new_tree(runner)?.current();
        // Escape-heavy: 1 seed in 8, by its top bits (no extra draw).
        let escapes = self.escapes && seed >> 61 == 0;
        Ok(TextTree { len: Len::new(self.size.min, len), seed, escapes })
    }
}

/// A [`Text`] value as it shrinks: its length, toward the minimum.
#[derive(Clone, Debug)]
pub struct TextTree {
    len: Len,
    seed: u64,
    escapes: bool,
}

/// A size as it shrinks: binary search down to the minimum.
#[derive(Clone, Debug)]
struct Len {
    min: usize,
    above: BinarySearch,
}

impl Len {
    fn new(min: usize, n: usize) -> Self {
        Len { min, above: BinarySearch::new(n - min) }
    }

    fn current(&self) -> usize {
        self.min + self.above.current()
    }

    fn simplify(&mut self) -> bool {
        self.above.simplify()
    }

    fn complicate(&mut self) -> bool {
        self.above.complicate()
    }
}

impl ValueTree for TextTree {
    type Value = String;

    fn current(&self) -> String {
        let (seed, len) = (self.seed, self.len.current());
        if self.escapes { fill_from(ESCAPES, seed, len) } else { fill(seed, len) }
    }

    fn simplify(&mut self) -> bool {
        self.len.simplify()
    }

    fn complicate(&mut self) -> bool {
        self.len.complicate()
    }
}

/// What JSON escapes: the control characters, `"` and `\`.
const ESCAPES: &[u8] = b"\x00\x01\x02\x03\x04\x05\x06\x07\x08\t\n\x0b\x0c\r\x0e\x0f\x10\x11\x12\
    \x13\x14\x15\x16\x17\x18\x19\x1a\x1b\x1c\x1d\x1e\x1f\"\\";

/// `len` bytes of `[a-z0-9]`, from `seed`.
pub(crate) fn fill(seed: u64, len: usize) -> String {
    fill_from(b"abcdefghijklmnopqrstuvwxyz0123456789", seed, len)
}

/// `len` bytes of `CHARS` (ASCII), from `seed`.
fn fill_from(chars: &[u8], seed: u64, len: usize) -> String {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            // xorshift64
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            chars[(x % chars.len() as u64) as usize] as char
        })
        .collect()
}

/// Lists of `element`: 0 to 64 items, the count favoring the edges.
pub fn list<S: Strategy>(element: S) -> List<S> {
    List { element, size: Size { min: 0, max: LIST_MAX } }
}

/// The strategy [`list`] returns.
#[derive(Clone, Debug)]
#[must_use]
pub struct List<S> {
    element: S,
    size: Size,
}

impl<S> List<S> {
    /// At most `max` items: a batch limit you want to probe past.
    pub fn max(mut self, max: usize) -> Self {
        self.size.max = max;
        self.size.min = self.size.min.min(max);
        self
    }

    /// At least `min` items.
    #[track_caller]
    pub fn min(mut self, min: usize) -> Self {
        assert!(min <= self.size.max, "input: min {min} above max {}", self.size.max);
        self.size.min = min;
        self
    }
}

impl<S: Strategy> Strategy for List<S> {
    type Tree = ListTree<S::Tree>;
    type Value = Vec<S::Value>;

    fn new_tree(&self, runner: &mut TestRunner) -> NewTree<Self> {
        let n = self.size.draw(runner)?;
        let items = (0..n).map(|_| self.element.new_tree(runner)).collect::<Result<_, _>>()?;
        Ok(ListTree { items, len: Len::new(self.size.min, n), at: None, item: 0 })
    }
}

/// A [`List`] value as it shrinks: its count, then each item.
pub struct ListTree<T> {
    items: Vec<T>,
    len: Len,
    /// What the last `simplify` changed: `None` the count, else an item.
    at: Option<usize>,
    /// The next item to simplify.
    item: usize,
}

impl<T: fmt::Debug> fmt::Debug for ListTree<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ListTree").field("len", &self.len.current()).finish_non_exhaustive()
    }
}

impl<T: ValueTree> ValueTree for ListTree<T> {
    type Value = Vec<T::Value>;

    fn current(&self) -> Self::Value {
        self.items[..self.len.current()].iter().map(ValueTree::current).collect()
    }

    fn simplify(&mut self) -> bool {
        if self.at.is_none() && self.item == 0 && self.len.simplify() {
            return true;
        }
        while self.item < self.len.current() {
            if self.items[self.item].simplify() {
                self.at = Some(self.item);
                return true;
            }
            self.item += 1;
        }
        false
    }

    fn complicate(&mut self) -> bool {
        match self.at {
            None => self.len.complicate(),
            Some(i) => self.items[i].complicate(),
        }
    }
}

/// The form a JSON message or a counterparty's reply takes.
///
/// ```
/// use near_contract_sim::explore::input::Shape;
/// let msg = serde_json::json!({ "Swap": { "min_out": "5" } });
/// assert_eq!(Shape::Valid.json(&msg), r#"{"Swap":{"min_out":"5"}}"#);
/// assert_eq!(Shape::Empty.json(&msg), "");
/// assert_eq!(Shape::Malformed.json(&msg), r#"{"Swap":{"min_out":"5"}"#);
/// assert_eq!(Shape::Unknown.json(&msg), r#"{"Unknown":{"min_out":"5"}}"#);
/// assert!(Shape::Huge.json(&msg).len() > 65536);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Shape {
    /// The value as JSON.
    Valid,
    /// No bytes at all.
    Empty,
    /// The JSON cut short by its last byte.
    Malformed,
    /// An unknown variant tag, an unknown field, or the value wrapped as
    /// `{"unknown": ..}`.
    Unknown,
    /// The JSON followed by 64 KiB of whitespace.
    Huge,
    /// The JSON padded with whitespace to `n` bytes
    /// ([`Reply::pad_to`](crate::Reply::pad_to) as a reply).
    Bytes(usize),
    /// The JSON padded to `max_length_returned_data`, the largest reply
    /// ([`Reply::at_limit`](crate::Reply::at_limit) as a reply).
    Limit,
}

impl Shape {
    /// `value` as JSON in this shape. `()` is no bytes, not `null`, as in
    /// [`Reply::json(())`](crate::Reply::json).
    #[track_caller]
    pub fn json(self, value: impl Serialize) -> String {
        let void = std::mem::size_of_val(&value) == 0;
        let value = serde_json::to_value(value).expect("a JSON value");
        let void = void && value.is_null();
        match self {
            Shape::Valid if void => String::new(),
            Shape::Valid => value.to_string(),
            Shape::Empty => String::new(),
            Shape::Malformed => {
                let mut s = value.to_string();
                match s.len() {
                    0 | 1 => "{".into(),
                    _ => {
                        s.pop();
                        s
                    }
                }
            }
            Shape::Unknown => match value {
                serde_json::Value::Object(m) if m.len() == 1 => {
                    let (_, v) = m.into_iter().next().expect("one key");
                    serde_json::json!({ "Unknown": v }).to_string()
                }
                serde_json::Value::Object(mut m) => {
                    m.insert("unknown".into(), true.into());
                    serde_json::Value::Object(m).to_string()
                }
                other => serde_json::json!({ "unknown": other }).to_string(),
            },
            Shape::Huge => value.to_string() + &" ".repeat(HUGE),
            Shape::Bytes(n) => {
                let s = value.to_string();
                let pad = n.saturating_sub(s.len());
                s + &" ".repeat(pad)
            }
            Shape::Limit => Shape::Bytes(default_limit()).json(value),
        }
    }
}

fn default_limit() -> usize {
    let config = crate::chain::runtime_config(near_primitives::version::PROTOCOL_VERSION);
    usize::try_from(config.wasm_config.limit_config.max_length_returned_data).unwrap_or(usize::MAX)
}

/// A [`Shape`]: `Valid` in half the draws, shrinking toward it.
pub fn shape() -> impl Strategy<Value = Shape> + Clone {
    prop_oneof![
        4 => Just(Shape::Valid),
        1 => Just(Shape::Empty),
        1 => Just(Shape::Malformed),
        1 => Just(Shape::Unknown),
        1 => Just(Shape::Huge),
    ]
}

/// What a counterparty stub does with a call: return a value in a
/// [`Shape`], or panic. Bind its [`reply`](Self::reply) with
/// [`Tx::stub_reply`](crate::Tx::stub_reply).
///
/// Held in an op's field as `Returns(_)` and bound in `submit`
/// (`tx.stub_reply(stub, method, answer.reply(value))`), it is a choice a
/// scan tries: `Panic`, and each `Shape` inside `Returns`
/// (`Returns(Shape::Huge)` too). A witness holding `Panic` varies
/// nothing, since no plain value builds a `Shape`. A reply bound in the
/// fixture, or a field left `None`, takes no other shape: move the
/// `Answer` into the op to vary it.
///
/// ```
/// use near_contract_sim::explore::input::{Answer, Shape};
/// use near_contract_sim::Reply;
/// assert_eq!(Answer::Returns(Shape::Valid).reply(["5"]), Reply::json(["5"]));
/// assert_eq!(Answer::Panic.reply(["5"]), Reply::panic("the counterparty refuses"));
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Answer {
    /// Returns the value, written in this shape.
    Returns(Shape),
    /// Panics instead: the call fails.
    Panic,
}

impl Answer {
    /// The stub's reply: `value` in its shape, or a panic.
    #[track_caller]
    pub fn reply(self, value: impl Serialize) -> crate::Reply {
        match self {
            Answer::Returns(Shape::Limit) => {
                crate::Reply::bytes(Shape::Valid.json(value)).at_limit()
            }
            Answer::Returns(Shape::Bytes(n)) => {
                crate::Reply::bytes(Shape::Valid.json(value)).pad_to(n)
            }
            Answer::Returns(shape) => crate::Reply::bytes(shape.json(value)),
            Answer::Panic => crate::Reply::panic("the counterparty refuses"),
        }
    }
}

/// An [`Answer`]: a value in a [`shape`] in 8 draws of 10, the largest
/// reply ([`Shape::Limit`]) in 1, else a panic. A message can't take
/// `Limit`: it is past a transaction's size.
pub fn answer() -> impl Strategy<Value = Answer> + Clone {
    prop_oneof![
        8 => shape().prop_map(Answer::Returns),
        1 => Just(Answer::Returns(Shape::Limit)),
        1 => Just(Answer::Panic),
    ]
}

/// A gas-valued field, such as `min_gas`, as
/// [`gas::mixed`](super::gas::mixed) draws a call's gas: a threshold in
/// half the draws, where a floor comparison is wrong.
pub fn gas() -> impl Strategy<Value = near_primitives::types::Gas> + Clone {
    super::gas::field()
}

/// Which existing thing an op names (a held token), resolved when it
/// submits against the set it reads ([`of`](Self::of)), so a drawn plan
/// names something real.
///
/// An op that consumes what an earlier step made (a request to redeem, a
/// queued id) picks it by index from the set it reads at submit: a scan
/// varies one step and doesn't re-pair its partner, so a name fixed in
/// the plan misses the partner's new output. With nothing to pick the op
/// skips. If its own edge moved, a scan lists that as an open
/// `incomplete:` item, which a blanket `.excuse("incomplete:*", ..)`
/// hides; a later step's skip is what the edge did, and isn't listed. A
/// `Pick`'s edges wrap to real items, so keep a witness with a fixed,
/// unknown id where a missing one is a case to judge.
///
/// ```
/// use near_contract_sim::explore::input::Pick;
/// let held = ["a.near", "b.near", "c.near"];
/// assert_eq!(Pick(0).of(&held), Ok(&"a.near"));
/// assert_eq!(Pick(4).of(&held), Ok(&"b.near")); // wraps around
/// assert!(Pick(0).of::<&str>(&[]).is_err()); // `?` makes it a Skip
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Pick(pub u32);

impl Pick {
    /// The item at this index, wrapping; an error (a
    /// [`Skip`](super::Skip) with `?`) if there are none.
    pub fn of<T>(self, items: &[T]) -> Result<&T, PickError> {
        match items.len() {
            0 => Err(PickError(self)),
            n => Ok(&items[self.0 as usize % n]),
        }
    }
}

/// A [`Pick`] from an empty set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PickError(Pick);

/// `Pick(3) of none`.
impl fmt::Display for PickError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?} of none", self.0)
    }
}

impl std::error::Error for PickError {}

/// A [`Pick`], favoring the first few; shrinks toward the first.
pub fn pick() -> impl Strategy<Value = Pick> + Clone {
    prop_oneof![Just(0u32), 1u32..8, any::<u32>()].prop_map(Pick)
}

/// A plan's op as `{:?}` shows it, long strings and lists cut:
/// `"k3j9x0aa…" (16384 bytes)`. The replay file keeps every byte.
pub(crate) fn compact(debug: &str) -> String {
    const LONG: usize = 24;
    let chars: Vec<char> = debug.chars().collect();
    let mut out = String::with_capacity(debug.len().min(4096));
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '"' => {
                // A string literal: to the closing quote, escapes skipped.
                let mut j = i + 1;
                let mut len = 0;
                while j < chars.len() && chars[j] != '"' {
                    j += if chars[j] == '\\' { 2 } else { 1 };
                    len += 1;
                }
                let lit: String = chars[i..=j.min(chars.len() - 1)].iter().collect();
                if len > LONG {
                    let head: String = chars[i + 1..].iter().take(8).collect();
                    out.push_str(&format!("\"{head}…\" ({len} bytes)"));
                } else {
                    out.push_str(&lit);
                }
                i = j + 1;
            }
            '[' => {
                let (items, end) = list_items(&chars, i);
                if items.len() > 8 {
                    let first: Vec<String> = items[..3].iter().map(|s| compact(s.trim())).collect();
                    out.push_str(&format!("[{}, … {} items]", first.join(", "), items.len()));
                    i = end + 1;
                } else {
                    out.push('[');
                    i += 1;
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// The top-level items of the list opening at `chars[open]`, and its end.
fn list_items(chars: &[char], open: usize) -> (Vec<String>, usize) {
    let (mut items, mut item, mut depth, mut i) = (vec![], String::new(), 0usize, open + 1);
    while i < chars.len() {
        let c = chars[i];
        match c {
            '"' => {
                let mut j = i + 1;
                while j < chars.len() && chars[j] != '"' {
                    j += if chars[j] == '\\' { 2 } else { 1 };
                }
                item.extend(&chars[i..=j.min(chars.len() - 1)]);
                i = j + 1;
                continue;
            }
            '[' | '{' | '(' => depth += 1,
            ']' if depth == 0 => {
                if !item.trim().is_empty() {
                    items.push(item);
                }
                return (items, i);
            }
            ']' | '}' | ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                items.push(std::mem::take(&mut item));
                i += 1;
                continue;
            }
            _ => {}
        }
        item.push(c);
        i += 1;
    }
    (vec![], chars.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draws<S: Strategy>(s: &S, n: usize) -> Vec<S::Value> {
        let mut runner = TestRunner::deterministic();
        (0..n).map(|_| s.new_tree(&mut runner).unwrap().current()).collect()
    }

    /// A default answer is the largest reply in about one draw in ten,
    /// so a limit-size reply needs no sweep; a message's shape never is:
    /// it would be past a transaction's size.
    #[test]
    fn a_default_answer_is_sometimes_the_largest_reply() {
        let limit = Answer::Returns(Shape::Limit);
        let n = draws(&answer(), 1000).into_iter().filter(|a| *a == limit).count();
        assert!((60..=140).contains(&n), "{n} of 1000");
        assert!(!draws(&shape(), 1000).contains(&Shape::Limit));
    }

    /// Half the draws are edges: the host sizes and the maximum come up
    /// often, and the rest stay mostly small.
    #[test]
    fn text_lengths_favor_the_edges() {
        let lens: Vec<usize> = draws(&text(), 4000).iter().map(String::len).collect();
        assert!(lens.iter().all(|&n| n <= TEXT_MAX));
        let count = |f: &dyn Fn(usize) -> bool| lens.iter().filter(|&&n| f(n)).count();
        // 12 edge slots (0, 1, a few, 63..65, 2047..2049, 16383..16385):
        // about 170 draws each.
        assert!((380..640).contains(&count(&|n| n >= 16383)), "{}", count(&|n| n >= 16383));
        assert!(count(&|n| n == 16385) > 100, "one byte past the log limit");
        assert!((350..700).contains(&count(&|n| (2047..=2049).contains(&n))));
        // Empty only at the edges: one slot of 12.
        assert!((120..260).contains(&count(&|n| n == 0)), "{}", count(&|n| n == 0));
        // Log-uniform elsewhere: most draws are short.
        assert!(count(&|n| n <= 100) > 1600, "{}", count(&|n| n <= 100));
        let ids: Vec<String> = draws(&id(), 1000);
        assert!(ids.iter().all(|s| s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric())));
        assert!(ids.iter().filter(|s| s.len() == 64).count() > 50);
        // 1 draw in 8 is escape-heavy: control characters, quotes and
        // backslashes, nothing else.
        let all: Vec<String> = draws(&text().min(1), 800);
        let escaping = |s: &&String| s.bytes().all(|b| b < 0x20 || b == b'"' || b == b'\\');
        let n = all.iter().filter(escaping).count();
        assert!((60..=140).contains(&n), "{n} of 800 escape-heavy");
        assert!(all.iter().all(|s| escaping(&s) || s.bytes().all(|b| b.is_ascii_alphanumeric())));
        let capped: Vec<String> = draws(&text().max(10), 200);
        assert!(capped.iter().all(|s| s.len() <= 10));
        assert!(draws(&id().min(2), 200).iter().all(|s| s.len() >= 2));
    }

    /// A list's max is the field's: past the default 64, its edges are
    /// the new maximum and one below.
    #[test]
    fn a_list_max_moves_its_edges() {
        let lens: Vec<usize> =
            draws(&list(Just(0u8)).max(1000), 2000).iter().map(Vec::len).collect();
        assert!(lens.iter().all(|&n| n <= 1000));
        let at = |n: usize| lens.iter().filter(|&&l| l == n).count();
        assert!(at(1000) > 60 && at(999) > 60, "{} {}", at(1000), at(999));
    }

    #[test]
    fn list_counts_favor_the_edges_and_items_vary() {
        let lists = draws(&list(id()), 2000);
        let count = |n: usize| lists.iter().filter(|l| l.len() == n).count();
        assert!(lists.iter().all(|l| l.len() <= LIST_MAX));
        assert!(count(64) > 100 && count(63) > 100 && count(0) > 100, "{}", count(64));
        let long = lists.iter().find(|l| l.len() > 10).unwrap();
        let same_len: Vec<&String> = long.iter().filter(|s| s.len() == long[0].len()).collect();
        // Two strings of one length differ, unless so short they can't.
        assert!(same_len.windows(2).all(|w| w[0] != w[1] || w[0].len() < 4), "{long:?}");
    }

    /// A raised maximum is drawn itself: a batch cap of 112 is reached at
    /// 112 and 111, past the default 64.
    #[test]
    fn a_raised_list_maximum_is_drawn_at_its_edge() {
        let lists = draws(&list(Just(0u8)).max(112), 2000);
        let count = |n: usize| lists.iter().filter(|l| l.len() == n).count();
        assert!(lists.iter().all(|l| l.len() <= 112));
        assert!(count(112) > 60 && count(111) > 60, "{} {}", count(112), count(111));
        assert!(lists.iter().filter(|l| l.len() > 64).count() > 200);
    }

    /// Shrinking a length finds the smallest that still fails; a list
    /// drops items first, then shortens the ones left.
    #[test]
    fn sizes_shrink_to_the_smallest_that_still_fails() {
        let mut runner = TestRunner::deterministic();
        let mut tree = loop {
            let t = text().new_tree(&mut runner).unwrap();
            if t.current().len() >= 16383 {
                break t;
            }
        };
        let fails = |s: &String| s.len() > 5000;
        let mut best = tree.current();
        let mut more = tree.simplify();
        while more {
            let c = tree.current();
            more = if fails(&c) {
                best = c;
                tree.simplify()
            } else {
                tree.complicate()
            };
        }
        assert_eq!(best.len(), 5001);

        let mut tree = loop {
            let t = list(id()).new_tree(&mut runner).unwrap();
            if t.current().len() >= 40 && t.current().iter().filter(|s| s.len() > 30).count() > 3 {
                break t;
            }
        };
        let fails = |l: &Vec<String>| l.len() >= 3 && l.iter().any(|s| s.len() >= 10);
        let mut best = tree.current();
        let mut more = tree.simplify();
        while more {
            let c = tree.current();
            more = if fails(&c) {
                best = c;
                tree.simplify()
            } else {
                tree.complicate()
            };
        }
        assert!((3..=10).contains(&best.len()), "{best:?}");
        assert!(best.iter().filter(|s| s.len() >= 10).count() == 1, "{best:?}");
    }

    #[test]
    fn shapes_are_mostly_valid() {
        let shapes = draws(&shape(), 800);
        let valid = shapes.iter().filter(|s| **s == Shape::Valid).count();
        assert!((330..470).contains(&valid), "{valid}");
        assert_eq!(Shape::Malformed.json(5), "{");
        assert_eq!(
            Shape::Unknown.json(serde_json::json!({"a": 1, "b": 2})),
            r#"{"a":1,"b":2,"unknown":true}"#
        );
        assert_eq!(Shape::Unknown.json(["1"]), r#"{"unknown":["1"]}"#);
        let huge = Shape::Huge.json(["1"]);
        assert_eq!(serde_json::from_str::<Vec<String>>(&huge).unwrap(), ["1"]);
    }

    #[test]
    fn long_strings_and_lists_print_compactly() {
        let memo = "m".repeat(16384);
        let ids: Vec<String> = (0..20).map(|i| format!("id{i}")).collect();
        let op = format!("{:?}", (&memo, &ids, "short \"q\""));
        assert_eq!(
            compact(&op),
            r#"("mmmmmmmm…" (16384 bytes), ["id0", "id1", "id2", … 20 items], "short \"q\"")"#
        );
        let nested = format!("{:?}", vec![vec![1u8; 3]; 9]);
        assert_eq!(compact(&nested), "[[1, 1, 1], [1, 1, 1], [1, 1, 1], … 9 items]");
        assert_eq!(compact("Deposit { n: 3 }"), "Deposit { n: 3 }");
    }

    /// A drawn pick shrinks toward the first thing; a drawn answer toward
    /// a valid reply.
    #[test]
    fn pick_and_answer_shrink_to_the_plain_case() {
        let mut runner = TestRunner::deterministic();
        for _ in 0..50 {
            let mut tree = pick().new_tree(&mut runner).unwrap();
            while tree.simplify() {}
            assert_eq!(tree.current(), Pick(0));
            let mut tree = answer().new_tree(&mut runner).unwrap();
            while tree.simplify() {}
            assert_eq!(tree.current(), Answer::Returns(Shape::Valid));
        }
    }
}
