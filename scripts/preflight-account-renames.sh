#!/bin/sh
# SHRINK S2 pre-cutover check: refuse while any account-rename record is unresolved.
#   scripts/preflight-account-renames.sh DATA_DIR
# The release that removes `account rename-backend` cannot finish such a record. A pre-cutover
# check only (the supervisor never refuses to start). Run this BEFORE switching the `current`
# link; resolve each listed id with the running release (`api recover <id>`, or
# `api abandon-operation --operation <id> --confirm <id>`). Exit 0: none unresolved.
set -eu
root=${1:-${SESSIONSATION_DATA_DIR:-}}
[ -n "$root" ] || { echo "usage: preflight-account-renames.sh DATA_DIR" >&2; exit 2; }
dir=$root/operations/account-renames
bad=0
for f in "$dir"/*.json; do
    [ -e "$f" ] || continue
    if ! grep -Eq '"phase"[[:space:]]*:[[:space:]]*"(complete|failed|abandoned)"' "$f"; then
        echo "UNRESOLVED account rename: $(basename "$f" .json)" >&2
        bad=1
    fi
done
[ "$bad" -eq 0 ] || { echo "PREFLIGHT FAIL: resolve the records above, then retry the cutover" >&2; exit 1; }
echo "PREFLIGHT OK: no unresolved account-rename records in $dir"
