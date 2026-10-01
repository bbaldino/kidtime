#!/bin/bash
# Records W press/release on the physical keyboard and on a user's Sunshine
# passthrough keyboard, with timestamps, to see where a stuck W comes from.
# Usage: dev/stuck-key-check.sh <stream-user> [seconds]   (default 60)
# Needs root, or membership in the input group. Reproduce the stuck key meanwhile.

STREAM_USER=${1:?usage: stuck-key-check.sh <stream-user> [seconds]}
SECS=${2:-60}
# The physical keyboard: the first keyboard-capable device on seat0 that isn't a Sunshine passthrough
LOCAL_NAME=${LOCAL_KEYBOARD:-$(for d in /sys/class/input/event*; do
    n=$(cat "$d/device/name"); dev=/dev/input/$(basename "$d")
    udevadm info -q property -n "$dev" | grep -q '^ID_INPUT_KEYBOARD=1' || continue
    udevadm info -q property -n "$dev" | grep -q '^ID_SEAT=seat-sunshine' && continue
    echo "$n"; break
done)}
find_dev() { for d in /sys/class/input/event*; do [ "$(cat "$d/device/name")" = "$1" ] && echo "/dev/input/$(basename "$d")"; done; }
LOCAL=$(find_dev "$LOCAL_NAME")
STREAM=$(find_dev "Keyboard passthrough ($STREAM_USER)")
echo "local keyboard: $LOCAL_NAME (${LOCAL:-not found})   $STREAM_USER's stream keyboard: ${STREAM:-not found}"
echo "Recording W for ${SECS}s. Note the time when the key seems stuck."
echo

exec timeout "$SECS" python3 - "LOCAL=$LOCAL" "STREAM=$STREAM" <<'EOF'
import os, select, struct, sys, time
# struct input_event on 64-bit: timeval (2 x long), type (u16), code (u16), value (s32)
EVENT = struct.Struct("llHHi")
EV_KEY, KEY_W = 1, 17
STATES = {0: "released", 1: "pressed", 2: "repeat"}

devices = {}
for arg in sys.argv[1:]:
    label, _, path = arg.partition("=")
    if path:
        devices[os.open(path, os.O_RDONLY)] = label
last = {}
while True:
    for fd in select.select(list(devices), [], [])[0]:
        data = os.read(fd, EVENT.size * 64)
        for i in range(0, len(data) - EVENT.size + 1, EVENT.size):
            _, _, etype, code, value = EVENT.unpack_from(data, i)
            if etype != EV_KEY or code != KEY_W:
                continue
            label = devices[fd]
            # Show every press/release; for held keys only the first repeat
            if value == 2 and last.get(label) == 2:
                continue
            last[label] = value
            stamp = time.strftime("%H:%M:%S") + f".{int(time.time() * 1000) % 1000:03d}"
            print(f"{stamp}  {label:<6} {STATES.get(value, value)}", flush=True)
EOF
