#!/usr/bin/env bash
# An old transaction's yield can be resumed by a later transaction's timeout.
# Keep its RPC history until those callbacks finish; the image's 5 epochs can
# prune the parent in 240-300 blocks, even while its receipts are still pending.
set -euo pipefail
container=${1:?usage: retain-history.sh CONTAINER}
config=$(mktemp)
trap 'rm -f "$config"' EXIT
docker cp "$container:/data/config.json" "$config"
python3 - "$config" <<'PY'
import json
import sys
from pathlib import Path
path = Path(sys.argv[1])
config = json.loads(path.read_text())
config['gc_num_epochs_to_keep'] = 100
path.write_text(json.dumps(config, indent=2) + '\n')
PY
docker cp "$config" "$container:/data/config.json"
docker restart "$container" >/dev/null
