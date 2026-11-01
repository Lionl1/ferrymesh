#!/usr/bin/env bash
# Three ferry processes on this machine, in a line: alice -- bob -- carol.
# Alice and Carol know only Bob's address, so their messages go through Bob,
# who relays them without being able to read them.
set -euo pipefail

cargo build --quiet --release -p ferrymesh-cli
FERRY="$PWD/target/release/ferry"
DIR="$(mktemp -d)"
# SIGINT lets every node save its state and exit cleanly.
trap 'kill -INT $(jobs -p) 2>/dev/null || true; wait; rm -rf "$DIR"' EXIT

for who in alice bob carol; do
  "$FERRY" --home "$DIR/$who" init >/dev/null
done
card() { "$FERRY" --home "$DIR/$1" id | awk '/^card:/ {print $2}'; }
"$FERRY" --home "$DIR/alice" contact add carol "$(card carol)" >/dev/null
"$FERRY" --home "$DIR/carol" contact add alice "$(card alice)" >/dev/null

# Written while offline: Alice's node carries it until it meets someone.
"$FERRY" --home "$DIR/alice" send carol "hello Carol, this went through Bob"

"$FERRY" --home "$DIR/bob" run --bind 127.0.0.1 --port 47601 --no-broadcast \
  </dev/null >"$DIR/bob.log" 2>&1 &
"$FERRY" --home "$DIR/carol" run --bind 127.0.0.1 --port 47602 --no-broadcast \
  --peer 127.0.0.1:47601 </dev/null >"$DIR/carol.log" 2>&1 &
"$FERRY" --home "$DIR/alice" run --bind 127.0.0.1 --port 47600 --no-broadcast \
  --peer 127.0.0.1:47601 </dev/null >"$DIR/alice.log" 2>&1 &

for _ in $(seq 1 100); do
  if grep -q "<- alice" "$DIR/carol.log" && grep -q "delivered" "$DIR/alice.log"; then
    echo "--- carol"; cat "$DIR/carol.log"
    echo "--- alice"; cat "$DIR/alice.log"
    echo "--- bob (relay only)"; cat "$DIR/bob.log"
    exit 0
  fi
  sleep 0.2
done
echo "no delivery within 20 s" >&2
tail -n +1 "$DIR"/*.log >&2
exit 1
