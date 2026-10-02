#!/bin/bash
# Install or upgrade kidtime-agent on this machine, as a systemd service.
#
#   cargo build --release -p agent        # as yourself, not root
#   sudo deploy/install-agent.sh --server http://SERVER-HOST:8470 --users kid1,kid2
#
# Options:
#   --server URL       the kidtime server (required on first install)
#   --users A,B        accounts to track (required on first install)
#   --host NAME        name to report; needed when the hostname is unset or generic
#   --streaming        this machine hosts Sunshine streaming sessions
#   --binary PATH      default: target/release/kidtime-agent in this checkout
#   --token-file FILE  read the agent token from FILE instead of asking for it
#
# The token is asked for without echo and only written to /etc/kidtime/agent.toml
# (mode 600). Running it again with no options upgrades the binary and keeps the
# existing config.

set -euo pipefail

REPO=$(cd "$(dirname "$0")/.." && pwd)
# Test hook: install under another root and skip systemd.
ROOT=${KIDTIME_INSTALL_ROOT:-}
CONFIG=$ROOT/etc/kidtime/agent.toml

server= users= host= streaming=no token_file=
binary=$REPO/target/release/kidtime-agent
while [ $# -gt 0 ]; do
  case $1 in
    --server) server=${2:?--server needs a URL}; shift 2 ;;
    --users) users=${2:?--users needs a list}; shift 2 ;;
    --host) host=${2:?--host needs a name}; shift 2 ;;
    --streaming) streaming=yes; shift ;;
    --binary) binary=${2:?--binary needs a path}; shift 2 ;;
    --token-file) token_file=${2:?--token-file needs a path}; shift 2 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

fail() { echo "error: $*" >&2; exit 1; }

[ -n "$ROOT" ] || [ "$(id -u)" = 0 ] || fail "run this with sudo"
[ -x "$binary" ] || fail "no agent binary at $binary. Build it first: cargo build --release -p agent"

write_config=yes
if [ -z "$server$users$host$token_file" ] && [ "$streaming" = no ]; then
  [ -f "$CONFIG" ] || fail "first install: --server and --users are required"
  write_config=no
fi

if [ "$write_config" = yes ]; then
  [ -n "$server" ] || fail "--server is required"
  [ -n "$users" ] || fail "--users is required"
  server=${server%/}
  for u in ${users//,/ }; do
    id "$u" >/dev/null 2>&1 || fail "no such account on this machine: $u"
  done

  if [ -n "$token_file" ]; then
    token=$(tr -d '[:space:]' < "$token_file")
  else
    read -rsp "Agent token (KIDTIME_AGENT_TOKEN on the server): " token; echo
  fi
  [[ $token =~ ^[A-Za-z0-9._~+/=-]+$ ]] || fail "the token is empty or has unexpected characters"

  # Check the server and the token now, rather than finding out from the journal.
  health=$(curl -s -m 5 "$server/healthz" || true)
  [ "$health" = ok ] || fail "$server/healthz did not answer 'ok' (got: '${health:0:60}')"
  probe='{"host":"install-check","agent_id":"install-check","interval_secs":15,"samples":[]}'
  code=$(curl -s -m 5 -o /dev/null -w '%{http_code}' -X POST "$server/api/report" \
    -H "Authorization: Bearer $token" -H 'Content-Type: application/json' -d "$probe" || true)
  [ "$code" != 401 ] || fail "the server rejected the token"
  [[ $code == 2?? ]] || echo "warning: test report got HTTP $code (expected 2xx); continuing" >&2

  quoted_users=$(printf '"%s", ' ${users//,/ })
  install -d -m 755 "$ROOT/etc/kidtime"
  (
    umask 077
    {
      echo "# Written by deploy/install-agent.sh. Holds the agent token: keep it mode 600."
      echo "server_url = \"$server\""
      echo "token = \"$token\""
      echo "users = [${quoted_users%, }]"
      [ -z "$host" ] || echo "host = \"$host\""
      if [ "$streaming" = yes ]; then
        echo 'streaming_units = ["sway-sunshine.service"]'
        echo 'streaming_sway_socket = "/run/user/{uid}/sway-sunshine.sock"'
      fi
    } > "$CONFIG.new"
  )
  mv "$CONFIG.new" "$CONFIG"
  echo "wrote $CONFIG"
fi

install -D -m 755 "$binary" "$ROOT/usr/local/bin/kidtime-agent"
install -D -m 644 "$REPO/deploy/kidtime-agent.service" "$ROOT/etc/systemd/system/kidtime-agent.service"
echo "installed /usr/local/bin/kidtime-agent and the systemd unit"

[ -z "$ROOT" ] || exit 0

systemctl daemon-reload
systemctl enable kidtime-agent.service >/dev/null
systemctl restart kidtime-agent.service
sleep 4

echo
echo "== service (expect: active)"
systemctl is-active kidtime-agent.service || true
echo "== what the agent sees now (expect one line per tracked user)"
/usr/local/bin/kidtime-agent --config "$CONFIG" --dump 2>/dev/null |
  python3 -c 'import json,sys; [print(" ", u["user"], u["state"], [a["name"] for a in u["apps"]]) for u in json.load(sys.stdin)["users"]]' ||
  echo "  (could not read the dump)"
echo "== recent log (expect no 'report failed' lines)"
journalctl -u kidtime-agent.service -n 8 --no-pager -o cat
