#!/bin/bash
# Checks the audio path of a user's Sunshine stream: game -> sink -> monitor -> Sunshine.
# Usage: sudo dev/stream-audio-check.sh <user>

USER_NAME=${1:?usage: stream-audio-check.sh <user>}
USER_ID=$(id -u "$USER_NAME") || exit 1
export XDG_RUNTIME_DIR=/run/user/$USER_ID
# Run as the user, since each user has their own PipeWire
as_user() {
    if [ "$(id -u)" = "$USER_ID" ]; then "$@"; else runuser -u "$USER_NAME" -- env XDG_RUNTIME_DIR="$XDG_RUNTIME_DIR" "$@"; fi
}
pa() { as_user pactl "$@"; }

# Mute and volume of every block in a `pactl list <kind>` listing
summarize() {
    pa list "$1" | awk -v kind="$2" '
        /^(Sink|Sink Input|Source Output) #/ { id=$NF }
        /^\s*Mute:/                           { mute=$2 }
        /^\s*Volume:/ && !vol                 { vol=$5 }
        /application.name =/                  { app=$0; sub(/.*= /, "", app) }
        /^\s*Name: / && kind=="sink"        { app=$2 }
        /^\s*Sink: / && kind=="stream"        { sink=$2 }
        /^\s*Source: / && kind=="capture"     { src=$2 }
        /^$/ && id { printf "  %-8s mute=%-3s vol=%-5s %s%s\n", id, mute, vol, app,
                        (sink ? " -> sink " sink : "") (src ? " <- source " src : "");
                     id=mute=vol=app=sink=src="" }
        END { if (id) printf "  %-8s mute=%-3s vol=%-5s %s%s\n", id, mute, vol, app,
                        (sink ? " -> sink " sink : "") (src ? " <- source " src : "") }'
}

echo "default sink: $(pa get-default-sink)"
echo "== sinks";              summarize sinks sink
echo "== playing streams";    summarize sink-inputs stream
echo "== capture streams";    summarize source-outputs capture

# The source Sunshine records from, and how loud it is right now
SRC=$(pa list source-outputs | awk '/^Source Output #/ {src=""} /^\s*Source: / {src=$2} /application.name = "sunshine"/ {print src; exit}')
if [ -z "$SRC" ]; then
    echo "== Sunshine isn't recording anything (no client connected?)"
    exit 0
fi
PEAK=$(as_user timeout 2 \
        parec -d "$SRC" --format=s16le --channels=2 2>/dev/null \
    | python3 -c 'import sys, array; a = array.array("h", sys.stdin.buffer.read()); print(max(map(abs, a)) if a else 0)')
echo "== level on source $SRC (what Sunshine records): peak $PEAK / 32767"
if [ "${PEAK:-0}" -gt 100 ]; then
    echo "   Audio reaches Sunshine. If the stream is silent, check the Moonlight client."
else
    echo "   Silence. Look above for mute=yes or a low vol, or the game isn't making sound."
fi
