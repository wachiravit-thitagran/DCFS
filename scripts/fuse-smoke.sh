#!/usr/bin/env bash
# Mount DCFS for real and exercise it with ordinary tools.
#
# Linux only: needs /dev/fuse and fuse3. Starts its own server, so set
# DATABASE_URL to a THROWAWAY database (unset uses the in-memory repository).
#
#   DATABASE_URL=postgresql://... scripts/fuse-smoke.sh
set -euo pipefail

if [[ ! -e /dev/fuse ]]; then
    echo "SKIP: /dev/fuse is missing; FUSE mounting needs Linux with fuse3." >&2
    exit 0
fi

export MASTER_KEY="${MASTER_KEY:-$(openssl rand -hex 32)}"
export API_TOKEN="${API_TOKEN:-$(openssl rand -hex 32)}"
export SERVER_ADDR="${SERVER_ADDR:-127.0.0.1:8080}"
export OBJECT_STORE_PATH="${OBJECT_STORE_PATH:-$(mktemp -d)}"
# Collect almost immediately, so the sweep is observable inside a test run.
export GC_RETENTION_SECS="${GC_RETENTION_SECS:-1}"
export RUST_LOG="${RUST_LOG:-info}"

mnt=$(mktemp -d)
work=$(mktemp -d)
server_pid=""

cleanup() {
    fusermount3 -u "$mnt" 2>/dev/null || true
    [[ -n "$server_pid" ]] && kill "$server_pid" 2>/dev/null || true
    rm -rf "$mnt" "$work" "$OBJECT_STORE_PATH" "${server_log:-}"
}
trap cleanup EXIT

PROFILE="${PROFILE:-debug}"
[ "$PROFILE" = release ] && flag=--release || flag=
cargo build $flag --bin dcfs-server --bin dcfs-fuse
bin="${CARGO_TARGET_DIR:-target}/$PROFILE"

server_log=$(mktemp)
"$bin/dcfs-server" > "$server_log" 2>&1 &
server_pid=$!

for _ in $(seq 1 60); do
    curl -sf "http://$SERVER_ADDR/health" >/dev/null && break
    sleep 1
done
curl -sf "http://$SERVER_ADDR/health" >/dev/null || {
    echo "server never became healthy" >&2
    echo "--- dcfs-server log ---" >&2
    cat "$server_log" >&2
    exit 1
}

MODE="${MODE:-stream}"

mount_fs() {
    DCFS_TOKEN="$API_TOKEN" "$bin/dcfs-fuse" "$mnt" \
        --server "http://$SERVER_ADDR" --mode "$MODE" &
    for _ in $(seq 1 30); do
        mountpoint -q "$mnt" && return 0
        sleep 1
    done
    echo "mount never appeared at $mnt" >&2
    exit 1
}

mount_fs
echo "mounted at $mnt in $MODE mode"

check() { echo "  ok: $1"; }

# --- ordinary file operations ---------------------------------------------

mkdir "$mnt/docs" "$mnt/docs/nested"
[[ -d "$mnt/docs/nested" ]]
check "mkdir, nested directories"

echo "hello dcfs" > "$mnt/docs/hello.txt"
[[ "$(cat "$mnt/docs/hello.txt")" == "hello dcfs" ]]
check "write and read back a small file"

ls "$mnt/docs" | grep -q hello.txt
check "readdir lists the new file"

# A file larger than one chunk, checked by content hash rather than by eye.
head -c 5000000 /dev/urandom > "$work/big.bin"
cp "$work/big.bin" "$mnt/docs/big.bin"
[[ "$(sha256sum < "$work/big.bin" | cut -d' ' -f1)" == "$(sha256sum < "$mnt/docs/big.bin" | cut -d' ' -f1)" ]]
check "multi-chunk file copies with a matching checksum"

# Unaligned random write in the middle of the file.
dd if=/dev/zero of="$mnt/docs/big.bin" bs=1 seek=1234567 count=4096 conv=notrunc status=none
dd if=/dev/zero of="$work/big.bin" bs=1 seek=1234567 count=4096 conv=notrunc status=none
[[ "$(sha256sum < "$work/big.bin" | cut -d' ' -f1)" == "$(sha256sum < "$mnt/docs/big.bin" | cut -d' ' -f1)" ]]
check "unaligned overwrite matches the same edit on a local file"

mv "$mnt/docs/hello.txt" "$mnt/docs/nested/renamed.txt"
[[ -f "$mnt/docs/nested/renamed.txt" && ! -f "$mnt/docs/hello.txt" ]]
[[ "$(cat "$mnt/docs/nested/renamed.txt")" == "hello dcfs" ]]
check "rename moves the entry and keeps the bytes"

# A remount proves the data is durable in the metadata store, not in a cache.
fusermount3 -u "$mnt"
mount_fs
[[ "$(sha256sum < "$work/big.bin" | cut -d' ' -f1)" == "$(sha256sum < "$mnt/docs/big.bin" | cut -d' ' -f1)" ]]
check "checksum survives unmount and remount"

objects_before=$(find "$OBJECT_STORE_PATH" -type f | wc -l)
rm "$mnt/docs/big.bin"
[[ ! -f "$mnt/docs/big.bin" ]]
check "unlink removes the file"

# The bytes must leave the backend too, not just the namespace — the call that
# deletes a Discord attachment. How to see that depends on where they went:
# with Discord there is no local directory to count, so watch what the
# collector reported instead.
collected=0
for _ in $(seq 1 30); do
    if [[ -n "${DISCORD_WEBHOOK_ID:-}" ]]; then
        # Strip colour: a log going to a file still carries escape codes, and
        # they sit between the field name and its value.
        sed $'s/\033\[[0-9;]*m//g' "$server_log" \
            | grep -qE 'gc swept.*objects=[1-9]' && { collected=1; break; }
    else
        objects_now=$(find "$OBJECT_STORE_PATH" -type f | wc -l)
        [[ "$objects_now" -lt "$objects_before" ]] && { collected=1; break; }
    fi
    sleep 1
done
[[ "$collected" = 1 ]] || {
    echo "FAIL: garbage collection did not remove the deleted file's objects" >&2
    exit 1
}
check "garbage collection removed the deleted file's objects from the backend"

rmdir "$mnt/docs/nested" 2>/dev/null && { echo "FAIL: rmdir removed a non-empty directory" >&2; exit 1; }
check "rmdir refuses a non-empty directory"

rm "$mnt/docs/nested/renamed.txt"
rmdir "$mnt/docs/nested"
[[ ! -d "$mnt/docs/nested" ]]
check "rmdir removes an empty directory"

echo "FUSE smoke test passed in $MODE mode"
