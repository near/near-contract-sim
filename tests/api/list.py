"""The crate's public API, one line per nameable item, from rustdoc's HTML.

Usage: python3 -I tests/api/list.py <doc dir>/near_contract_sim

`tests/docs.rs` runs it after `cargo doc --no-deps --all-features` and
compares the output with `tests/api/public.txt`; with `NCS_API_WRITE=1`
it rewrites that file.

Each module page names its items and re-exports; each item page of this
crate names its fields, variants, methods, associated items and, for a
trait, its required and provided methods. Trait implementations are left
out, and so are the members of items re-exported from nearcore (their
names stay). `#[doc(hidden)]` items aren't in the HTML, so aren't listed.

An item of this crate, each of its members and each impl header before
the trait implementations carry their declaration as rustdoc prints it
(`= pub fn f(&self, n: u64)`), tags stripped and whitespace collapsed: a
changed type, bound or argument changes the line. A struct's, enum's or trait's own line stops at `{`;
its members follow on their own lines. Only pages a module page names are
read, so a page a removed item left in the doc dir is ignored.
"""
import html
import os
import re
import sys

root = sys.argv[1].rstrip("/")
crate = os.path.basename(root)
ITEM = re.compile(r"^(struct|enum|trait|fn|constant|type|union|macro|static)\.(\w+)\.html$")
ENTRY = re.compile(r'<dt><a class="(\w+)" href="[^"]*"[^>]*>(\w+)</a>')
REEXPORT = re.compile(r'id="reexport\.(\w+)"')
MEMBER = re.compile(
    r'id="(structfield|variant|method|tymethod|associatedconstant|associatedtype)\.([\w.]+)"'
)
IMPL = re.compile(r'<section id="impl-[^"]*" class="impl">')
# Methods a `Deref` target lends (a slice's): the target is named, its
# methods left to its own docs.
DEREF = re.compile(r'<h2 id="deref-methods[^"]*" class="section-header"><span>(.*?)</span>')
# A declaration: an item's, a member's header or a field's type.
DECL = re.compile(r'<pre class="rust item-decl"><code>(.*?)</code></pre>'
                  r'|<h[34] class="code-header">(.*?)</h[34]>|<code>(.*?)</code>', re.S)
# Sections after an item's own members.
END = re.compile(r'<h2 id="(trait-implementations|synthetic-implementations|'
                 r'blanket-implementations|implementors|dyn-compatibility|object-safety)"')


def decl(text, at):
    """The first declaration at or after `at`, as one line of plain text."""
    m = DECL.search(text, at)
    return plain(next(g for g in m.groups() if g is not None))


def plain(raw):
    """rustdoc's HTML for some code as one line of plain text."""
    raw = re.sub(r'<a [^>]*class="tooltip"[^>]*>ⓘ</a>', "", raw)  # a notable trait
    raw = re.sub(r"<(br|div)\b[^>]*>", " ", raw)  # a where clause's break
    text = " ".join(html.unescape(re.sub(r"<[^>]+>", "", raw)).split())
    # Undo rustdoc's wrapping of a long signature: `( a: u8, )` is `(a: u8)`.
    text = re.sub(r"([(<\[]) ", r"\1", text)
    return re.sub(r",? ([)>\]])", r"\1", text).rstrip(",")


out = set()
for d, dirs, files in os.walk(root):
    if "index.html" not in files:
        continue
    rel = os.path.relpath(d, root)
    path = crate if rel == "." else crate + "::" + rel.replace(os.sep, "::")
    # rustdoc breaks a long name with <wbr>: `Incoming<wbr>Call`.
    text = open(os.path.join(d, "index.html"), encoding="utf-8").read().replace("<wbr>", "")
    if "<title>Redirection</title>" in text:
        continue  # a private module's page, pointing at its re-export
    out.add(f"mod {path}")
    for name in REEXPORT.findall(text):
        out.add(f"use {path}::{name}")
    listed = set(ENTRY.findall(text))
    for kind, name in listed:
        if kind != "mod":
            out.add(f"{kind} {path}::{name}")
    for f in files:
        m = ITEM.match(f)
        if not m or m.groups() not in listed:
            continue  # a page a removed item left behind
        kind, name = m.groups()
        page = open(os.path.join(d, f), encoding="utf-8").read()
        head = page[page.find('class="main-heading"'):page.find("</rustdoc-toolbar>")+200]
        if "src/near_contract_sim/" not in head:
            continue  # a nearcore item re-exported: its name is listed above
        out.discard(f"{kind} {path}::{name}")
        item = decl(page, page.find('class="rust item-decl"'))
        if kind in ("struct", "enum", "trait", "union") and "{" in item:
            item = item[: item.find("{")].rstrip()
        out.add(f"{kind} {path}::{name} = {item}")
        end = END.search(page)
        own = page[: end.start()] if end else page
        for m in IMPL.finditer(own):
            out.add(f"{kind} {path}::{name} impl = {decl(own, m.end())}")
        deref = DEREF.search(own)
        lent = deref.start() if deref else len(own)
        if deref:
            out.add(f"{kind} {path}::{name} deref = {plain(deref.group(1))}")
        for m in MEMBER.finditer(own[:lent]):
            mk, mn = m.groups()
            line = f"{kind} {path}::{name} {mk} {mn}"
            out.add(line if mn.endswith(".fields") else f"{line} = {decl(own, m.end())}")
for line in sorted(out):
    print(line)
