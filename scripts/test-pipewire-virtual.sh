#!/usr/bin/env bash
# End-to-end test of Linux virtual devices against the PipeWire of this
# session (no audio hardware needed; PipeWire's dummy driver is enough):
#
#   pw-play 1 kHz tone -> "MixerCraft Game" (sink node) -> strip -> bus
#     -> "MixerCraft Chat" (source node) -> pw-record
#
# Checks that both nodes appear, keep running while idle (no stall
# recovery), carry the tone at unity gain, and disappear when the engine
# stops. Nothing is audible: no physical device is opened.
#
#   scripts/test-pipewire-virtual.sh [BIN_DIR]   (default: target/debug)
#
# Needs: a running PipeWire + WirePlumber, pw-play/pw-record/pw-cli
# (pipewire-bin), python3.
set -euo pipefail

bin="$(cd "${1:-target/debug}" && pwd)"
work="$(mktemp -d)"
engine_pid=""
cleanup() {
    [ -n "$engine_pid" ] && kill -INT "$engine_pid" 2>/dev/null || true
    rm -rf "$work"
}
trap cleanup EXIT
fail() { echo "FAIL: $*" >&2; echo "--- engine log" >&2; cat "$work/engine.log" >&2 || true; exit 1; }

cat > "$work/config.toml" <<'EOF'
schema_version = 1

[engine]
master_bus = "A1"

[[strips]]
name = "Game"
source = { kind = "device", id = "virtual:Game", name = "MixerCraft Game" }
routes = ["B1"]

[[buses]]
id = "A1"
name = "Meter only"

[[buses]]
id = "B1"
name = "Chat"
sink = { kind = "device", id = "virtual:Chat", name = "MixerCraft Chat" }
EOF

python3 - "$work/tone.wav" <<'EOF'
import math, struct, sys, wave
w = wave.open(sys.argv[1], "wb")
w.setnchannels(2); w.setsampwidth(2); w.setframerate(48000)
v = [int(8000 * math.sin(2 * math.pi * 1000 * i / 48000)) for i in range(48000 * 3)]
w.writeframes(b"".join(struct.pack("<hh", x, x) for x in v))
w.close()
EOF

MIXERCRAFT_LOG=info "$bin/mixercraftd" --config "$work/config.toml" > "$work/engine.log" 2>&1 &
engine_pid=$!

for _ in $(seq 50); do
    pw-cli ls Node 2>/dev/null | grep -q 'node.name = "mixercraft.source.chat"' && break
    sleep 0.2
done
nodes="$(pw-cli ls Node)"
grep -q 'node.name = "mixercraft.sink.game"' <<<"$nodes" || fail "sink node missing"
grep -q 'node.name = "mixercraft.source.chat"' <<<"$nodes" || fail "source node missing"
echo "ok: nodes mixercraft.sink.game (Audio/Sink) and mixercraft.source.chat (Audio/Source) exist"

# Idle longer than the engine's stall timeout: the nodes must keep running.
sleep 6
status="$("$bin/mixercraft" status)"
echo "$status" | grep -E "MixerCraft (Game|Chat) \("
[ "$(grep -cE 'running +MixerCraft (Game|Chat) \(' <<<"$status")" = 2 ] || fail "devices not running while idle"
grep -q "stalled" "$work/engine.log" && fail "a virtual device stalled while idle"
echo "ok: idle nodes keep running"

pw-record --target mixercraft.source.chat --rate 48000 --channels 2 --format s16 "$work/out.wav" &
rec=$!
sleep 1
pw-play --target mixercraft.sink.game "$work/tone.wav"
sleep 1
kill -INT "$rec"; wait "$rec" || true

python3 - "$work/out.wav" <<'EOF' || fail "tone did not arrive"
import math, struct, sys, wave
w = wave.open(sys.argv[1])
d = w.readframes(w.getnframes())
s = struct.unpack("<%dh" % (len(d) // 2), d)[::w.getnchannels()]
blk = 4800
rms = [math.sqrt(sum(x * x for x in s[i:i + blk]) / blk) for i in range(0, len(s) - blk, blk)]
loud = [r for r in rms if r > 5000]
expected = 8000 / math.sqrt(2)
print("recorded %.1f s, %d loud 100 ms blocks, median RMS %.0f (expected %.0f)" % (
    len(s) / 48000, len(loud), sorted(loud)[len(loud) // 2] if loud else 0, expected))
best = max(range(0, max(1, len(s) - 48000), 4800), key=lambda i: sum(abs(x) for x in s[i:i + 48000]))
seg = s[best:best + 48000]
zc = sum(1 for a, b in zip(seg, seg[1:]) if a < 0 <= b)
print("rising zero crossings in the loudest second: %d (1 kHz tone: ~1000)" % zc)
ok = len(loud) >= 25 and abs(sorted(loud)[len(loud) // 2] - expected) < expected * 0.05 and 990 <= zc <= 1010
sys.exit(0 if ok else 1)
EOF
echo "ok: tone passed sink -> strip -> bus -> source at unity gain"

kill -INT "$engine_pid"; wait "$engine_pid" || true
engine_pid=""
sleep 0.5
pw-cli ls Node | grep -q 'node.name = "mixercraft\.' && fail "nodes left behind after the engine stopped"
echo "ok: nodes removed with the engine"
echo "PASS"
