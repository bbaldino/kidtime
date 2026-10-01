#!/bin/bash
# Snapshot of a user's streaming session: what the agent reports, Sway's windows
# (focused one marked *), and the top GPU users in the session.
# Usage: sudo dev/stream-snapshot.sh <user>   (expects dev/agent.dev.toml)

USER_NAME=${1:?usage: stream-snapshot.sh <user>}
USER_ID=$(id -u "$USER_NAME") || exit 1
DEV=$(cd "$(dirname "$0")" && pwd)
CGROUP=/sys/fs/cgroup/user.slice/user-$USER_ID.slice/user@$USER_ID.service/app.slice/sway-sunshine.service

date +%T
echo "== agent"
"$DEV/../target/debug/kidtime-agent" --config "$DEV/agent.dev.toml" --dump \
  | python3 -c 'import json,sys; [print(u["user"], u["state"], [a["name"] for a in u["apps"]]) for u in json.load(sys.stdin)["users"] if u["user"]==sys.argv[1]]' "$USER_NAME"
echo "== sway windows (focused marked *)"
swaymsg -s "/run/user/$USER_ID/sway-sunshine.sock" -t get_tree -r 2>&1 \
  | python3 -c '
import json,sys
def walk(n):
    if n.get("pid") and n.get("type") in ("con","floating_con"):
        cls = n.get("app_id") or (n.get("window_properties") or {}).get("class")
        print("*" if n.get("focused") else " ", n["pid"], cls, "|", n.get("name"))
    for c in n.get("nodes",[])+n.get("floating_nodes",[]): walk(c)
walk(json.load(sys.stdin))' 2>&1
echo "== GPU use by process (gfx ns)"
for p in $(cat "$CGROUP/cgroup.procs"); do
  g=$(grep -h "drm-engine-gfx" /proc/$p/fdinfo/* 2>/dev/null | awk '{s+=$2} END {print s+0}')
  [ "$g" != 0 ] && echo "$g $(cat /proc/$p/comm)"
done | sort -rn | head -5
