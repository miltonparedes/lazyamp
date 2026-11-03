#!/usr/bin/env bash
# End-to-end check: a runner started outside lazyamp keeps every flag and its
# working directory when lazyamp restarts it (`r`, then `y`).
#
#   scripts/e2e.sh          # against examples/fake_amp.rs (no login, no network)
#   scripts/e2e.sh --real   # against the `amp` on PATH (needs a logged-in Amp)
#
# Linux only (reads /proc). Needs tmux.
set -euo pipefail

mode=fake
[[ ${1:-} == --real ]] && mode=real
repo=$(cd "$(dirname "$0")/.." && pwd)
tmp=$(mktemp -d)
id="lz-e2e-$$"
session="lazyamp-e2e-$$"
mkdir -p "$tmp/work" "$tmp/extra" "$tmp/src" "$tmp/bin" "$tmp/config" "$tmp/state"

say() { printf '\n== %s\n' "$*"; }
fail() {
	printf '\nFAIL: %s\n' "$*" >&2
	if tmux has-session -t "$session" 2>/dev/null; then
		echo '--- lazyamp screen ---' >&2
		tmux capture-pane -p -t "$session" >&2
	fi
	exit 1
}

# PIDs of `amp --no-tui --runner-id $id` processes (argv[0] named amp).
runner_pids() {
	python3 -I - "$id" <<'PY'
import glob, os, sys
for path in glob.glob("/proc/[0-9]*/cmdline"):
    try:
        argv = [a.decode(errors="replace") for a in open(path, "rb").read().split(b"\0") if a]
    except OSError:
        continue
    if (argv and os.path.basename(argv[0]) == "amp" and "--no-tui" in argv
            and any(a == sys.argv[1] and argv[i - 1] == "--runner-id" for i, a in enumerate(argv))):
        print(path.split("/")[2])
PY
}

cleanup() {
	tmux kill-session -t "$session" 2>/dev/null || true
	for pid in $(runner_pids); do kill "$pid" 2>/dev/null || true; done
	rm -rf "$tmp"
}
trap cleanup EXIT

wait_for() { # wait_for <seconds> <description> <command...>
	local secs=$1 what=$2
	shift 2
	for _ in $(seq $((secs * 2))); do
		"$@" && return 0
		sleep 0.5
	done
	fail "timed out after ${secs}s waiting for $what"
}

# Flags as order-independent units ("--flag value" pairs stay together).
normalized_flags() {
	python3 -I - "$1" <<'PY'
import sys
argv = [a.decode() for a in open(sys.argv[1], "rb").read().split(b"\0") if a][1:]
aliases = {"-m": "--mode"}
argv = [aliases.get(a, a) for a in argv]
units, i = [], 0
while i < len(argv):
    if i + 1 < len(argv) and not argv[i + 1].startswith("-"):
        units.append(argv[i] + " " + argv[i + 1]); i += 2
    else:
        units.append(argv[i]); i += 1
print("\n".join(sorted(units)))
PY
}

say "building lazyamp ($mode amp)"
cd "$repo"
cargo build --quiet --bin lazyamp
if [[ $mode == fake ]]; then
	cargo build --quiet --example fake_amp
	cp target/debug/examples/fake_amp "$tmp/bin/amp"
	amp="$tmp/bin/amp"
	# The fake accepts anything, so cover every flag lazyamp models plus unknown ones.
	flags=(--share --desktop --amp-env --share-network --features fast,pro -m high
		--log-file "$tmp/amp.log" --discover-dirs "$tmp/src" --future-flag)
else
	amp=$(command -v amp) || fail "amp is not on PATH"
	# No --share / --share-network: they expose this machine to the workspace.
	flags=(--desktop --amp-env --log-file "$tmp/amp.log")
fi
flags+=(--dir "$tmp/extra")

say "starting $id outside lazyamp: amp --no-tui --runner-id $id ${flags[*]}"
(cd "$tmp/work" && setsid -f "$amp" --no-tui --runner-id "$id" "${flags[@]}" \
	>"$tmp/runner.out" 2>&1 </dev/null)
listed() { "$amp" runner list --json 2>/dev/null | grep -q "\"$id\""; }
wait_for 60 "$id in amp runner list" listed
old_pid=$(runner_pids | head -1)
[[ -n $old_pid ]] || fail "runner process not found"
cp "/proc/$old_pid/cmdline" "$tmp/old.cmdline"
echo "pid $old_pid"

say "restarting it from lazyamp"
tmux new-session -d -s "$session" -x 120 -y 36 \
	"AMP_BIN='$amp' XDG_CONFIG_HOME='$tmp/config' XDG_STATE_HOME='$tmp/state' '$repo/target/debug/lazyamp'"
on_screen() { tmux capture-pane -p -t "$session" | grep -q "$1"; }
wait_for 30 "$id in lazyamp" on_screen "$id"
tmux send-keys -t "$session" r
wait_for 5 "restart confirmation" on_screen "Restart runner"
tmux send-keys -t "$session" y
restarted() {
	new_pid=$(runner_pids | grep -vx "$old_pid" | head -1)
	[[ -n $new_pid ]] && ! kill -0 "$old_pid" 2>/dev/null
}
wait_for 90 "a new runner process to replace pid $old_pid" restarted
wait_for 60 "$id listed again" listed
echo "pid $old_pid -> $new_pid"

say "comparing flags"
diff -u <(normalized_flags "$tmp/old.cmdline") <(normalized_flags "/proc/$new_pid/cmdline") ||
	fail "restart changed the runner's flags (- before, + after)"
normalized_flags "/proc/$new_pid/cmdline" | sed 's/^/  /'
new_cwd=$(readlink "/proc/$new_pid/cwd")
[[ $new_cwd == "$tmp/work" ]] || fail "cwd changed: $tmp/work -> $new_cwd"
echo "  cwd $new_cwd"

say "PASS"
