#!/usr/bin/env bash
# Run the pages' code in a consumer whose dependencies are the quickstart's
# block, then the README's layout, toml, test and commands verbatim beside a
# fresh cargo-near 0.22 contract, then enable near-kit.
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/ncs-consumer.XXXXXX")
pages=$(mktemp -d "${TMPDIR:-/tmp}/ncs-pages.XXXXXX")
trap 'rm -rf "$work" "$pages"' EXIT
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$root/target}"
sim_toolchain=$(sed -n 's/^channel = "\(.*\)"/\1/p' "$root/rust-toolchain.toml")
# The dependencies' opt-level of this repo's own test build: 0, or what CI
# sets in the environment, so the consumers share that target dir's nearcore.
deps_opt=$(printenv 'CARGO_PROFILE_DEV_PACKAGE_*_OPT_LEVEL' || echo 0)

# Every rust block of quickstart.md and the whole first harness of audit.md
# (tests/first_harness.rs), under the quickstart's own [dev-dependencies],
# the simulator's git source swapped for this checkout. The quickstart's
# fresh-build block has a file of its own and only compiles here.
mkdir -p "$pages/src" "$pages/tests/res"
python3 -I - "$root" "$pages" <<'PY'
from pathlib import Path
import re
import sys
root, out = map(Path, sys.argv[1:])
page = (root / 'skills/near-contract-sim/references/quickstart.md').read_text()
blocks = lambda lang: re.findall(rf'^```{lang}\n(.*?)^```', page, re.S | re.M)
# The pinned git source, its rev a placeholder or a commit.
git = r'\{ git = "https://github\.com/near/near-contract-sim", rev = "[^"]+" \}'
toml = blocks('toml')[0]
assert re.search(git, toml), toml
toml = re.sub(git, lambda _: f'{{ path = "{root}" }}', toml)
package = '[package]\nname = "consumer-pages"\nversion = "0.0.0"\nedition = "2024"\npublish = false\n\n'
(out / 'Cargo.toml').write_text(package + toml)
(out / 'src/lib.rs').write_text('')
tests = [b for b in blocks('rust') if 'CONTRACT_DIR' not in b]
fresh = [b for b in blocks('rust') if 'CONTRACT_DIR' in b]
assert len(tests) >= 6 and len(fresh) == 1, (len(tests), len(fresh))
(out / 'tests/quickstart.rs').write_text('\n'.join(tests))
prefix = '#![allow(dead_code)]\nconst CONTRACT_DIR: &str = env!("CARGO_MANIFEST_DIR");\n'
(out / 'tests/fresh_wasm.rs').write_text(prefix + fresh[0])
PY
cp "$root/tests/first_harness.rs" "$pages/tests/"
cp "$root"/tests/res/{fungible_token,vault,floor}.wasm "$pages/tests/res/"
(
    cd "$pages"
    # The page's opt-level = 2 is for speed; the gate builds nearcore as
    # this repo's tests do, so the two share a target dir's artifacts.
    cargo +"$sim_toolchain" test --config "profile.dev.package.\"*\".opt-level=$deps_opt"
)

# The README's layout: auction/ is cargo near new's exact template inputs,
# without its unrelated telemetry or generated deployment workflows (the
# template pinned with the CLI); auction-tests/ is the README's crate, its
# toml blocks and commands run verbatim, the simulator's git source swapped
# for this checkout. The parent directory's config builds nearcore
# unoptimized, as the pages above do, so the two share artifacts.
template=https://raw.githubusercontent.com/near/cargo-near/108a48d505148145b419cf1060dc5f3026881984/cargo-near/src/commands/new/new-project-template
mkdir -p "$work/auction/src" "$work/auction-tests/src" "$work/auction-tests/tests" \
    "$work/auction-tests/.cargo" "$work/.cargo"
for file in Cargo.template.toml rust-toolchain.toml src/lib.rs; do
    curl --fail --silent --show-error --location "$template/$file" -o "$work/auction/${file/template./}"
done
sed -i.bak 's/cargo-near-new-project-name/auction/' "$work/auction/Cargo.toml"
rm "$work/auction/Cargo.toml.bak"
printf '[profile.dev.package."*"]\nopt-level = %s\n' "$deps_opt" > "$work/.cargo/config.toml"
python3 -I - "$root" "$work/auction-tests" <<'PY'
from pathlib import Path
import re
import sys
root, out = map(Path, sys.argv[1:])
readme = (root / 'README.md').read_text()
blocks = lambda lang: re.findall(rf'^```{lang}\n(.*?)^```', readme, re.S | re.M)
layout = next(b for b in blocks('text') if 'auction-tests/' in b)
toml = {b.split('\n')[0]: b for b in blocks('toml')}
for file in ['.cargo/config.toml', 'Cargo.toml', 'rust-toolchain.toml', 'src/lib.rs', 'tests/sim.rs']:
    assert file in layout, (file, layout)
git = r'\{ git = "https://github\.com/near/near-contract-sim", rev = "[^"]+" \}'
manifest = toml['[package]']
assert re.search(git, manifest), manifest
(out / 'Cargo.toml').write_text(re.sub(git, lambda _: f'{{ path = "{root}" }}', manifest))
(out / 'rust-toolchain.toml').write_text(toml['[toolchain]'])
(out / '.cargo/config.toml').write_text(toml['[env]'])
(out / 'src/lib.rs').write_text('')
test = next(b for b in blocks('rust') if '\nfn bid() {' in b)
assert test == (root / 'tests/consumer/sim.rs').read_text(), 'README test != tests/consumer/sim.rs'
(out / 'tests/sim.rs').write_text(test)
commands = blocks('sh')[-1]
assert 'cargo test --locked --test sim' in commands, commands
(out / 'commands.sh').write_text(commands)
PY
rustup target add wasm32-unknown-unknown --toolchain "$sim_toolchain"
cp "$root/tests/consumer/fresh_wasm.rs" "$work/auction-tests/tests/"
cd "$work/auction-tests"
bash -euxo pipefail commands.sh
cargo test --test fresh_wasm -- --show-output

python3 -I - "$root" <<'PY'
from pathlib import Path
import sys
import tomllib
root = Path(sys.argv[1])
version = tomllib.loads((root / 'Cargo.toml').read_text())['dependencies']['near-kit']['version']
p = Path('Cargo.toml')
s = p.read_text().replace(f'near-contract-sim = {{ path = "{root}" }}', f'near-contract-sim = {{ path = "{root}", features = ["near-kit"] }}')
s = s.replace('[dev-dependencies]\n', f'[dev-dependencies]\nnear-kit = "{version}"\ntokio = {{ version = "1", features = ["macros", "rt", "test-util"] }}\n')
p.write_text(s)
PY
cp "$root/tests/consumer/near_kit.rs" tests/near_kit.rs
cargo test --test sim --test near_kit
