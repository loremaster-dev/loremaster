#!/bin/sh
# Sends one fixture hook event per verb to a sandboxed daemon and prints each result.
set -eu

if [ -z "${LOREMASTER_HOME:-}" ]; then
    echo "error: set LOREMASTER_HOME to a sandbox dir" >&2
    exit 1
fi
sandbox=$(printf '%s' "$LOREMASTER_HOME" | sed 's:/*$::')
if [ "$sandbox" = "${HOME:-}/.loremaster" ]; then
    echo "error: refusing to use the real Loremaster home" >&2
    exit 1
fi
if ! printf 'ping\n' | nc -U "$LOREMASTER_HOME/sock" >/dev/null 2>&1; then
    echo "error: daemon not running at \$LOREMASTER_HOME/sock" >&2
    exit 1
fi

root=$(cd "$(dirname "$0")/.." && pwd)
loremaster=${LOREMASTER_BIN:-"$root/target/debug/loremaster"}
db="$LOREMASTER_HOME/store.db"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

last_queued() {
    sqlite3 "$db" 'SELECT coalesce(max(id), 0) FROM turn_queue'
}

agent=claude-code
fixtures="$root/crates/loremaster-agents/tests/fixtures/$agent"
# A new session each run, so memories already injected in the last run appear again.
run_dir="$work/$agent"
mkdir -p "$run_dir"
cp "$fixtures/transcript.jsonl" "$run_dir/transcript.jsonl"
session="try-hook-$$-$agent"
size=$(wc -c <"$run_dir/transcript.jsonl" | tr -d ' ')
offset() {
    sqlite3 "$db" "SELECT transcript_offset FROM sessions WHERE session_id = '$session'"
}
for pair in SessionStart:session_start UserPromptSubmit:user_prompt_submit Stop:stop SessionEnd:session_end; do
    event=${pair%%:*}
    file=${pair##*:}
    echo "== $agent $event"
    before=$(last_queued)
    output=$(sed -e "s|__FIXTURE_DIR__|$run_dir|" \
        -e "s|\"session_id\": \"[^\"]*\"|\"session_id\": \"$session\"|" \
        "$fixtures/$file.json" | "$loremaster" hook "$agent" "$event")
    if [ -n "$output" ]; then
        echo "$output"
    else
        echo "(no output)"
    fi
    # Capture happens just after the reply, one commit per turn. The offset reaches the end of the
    # transcript with the last commit.
    if [ "$event" = Stop ]; then
        waited=0
        while [ "$(offset)" != "$size" ] && [ "$waited" -lt 300 ]; do
            sleep 0.1
            waited=$((waited + 1))
        done
    fi
    added=$(sqlite3 "$db" "SELECT count(*) FROM turn_queue WHERE id > $before")
    if [ "$added" -gt 0 ]; then
        echo "turn_queue +$added:"
        sqlite3 "$db" "SELECT turn FROM turn_queue WHERE id > $before ORDER BY id"
    fi
done
