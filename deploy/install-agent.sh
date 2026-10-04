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
#   --stop-timekpr     stop and disable timekpr's service on this machine, then exit
#   --uninstall        undo what the agent enforced (--release-all), then remove the agent,
#                      its config and its state, and the login-screen banner settings
#                      (the GDM profile override only if this script wrote it), and exit
#
# The token is asked for without echo and only written to /etc/kidtime/agent.toml
# (mode 600). Running it again with no options upgrades the binary and keeps the
# existing config.

set -euo pipefail

REPO=$(cd "$(dirname "$0")/.." && pwd)
# Test hook: install under another root and skip systemd.
ROOT=${KIDTIME_INSTALL_ROOT:-}
CONFIG=$ROOT/etc/kidtime/agent.toml

server= users= host= streaming=no token_file= stop_timekpr=no uninstall=no
binary=$REPO/target/release/kidtime-agent
while [ $# -gt 0 ]; do
  case $1 in
    --server) server=${2:?--server needs a URL}; shift 2 ;;
    --users) users=${2:?--users needs a list}; shift 2 ;;
    --host) host=${2:?--host needs a name}; shift 2 ;;
    --streaming) streaming=yes; shift ;;
    --binary) binary=${2:?--binary needs a path}; shift 2 ;;
    --token-file) token_file=${2:?--token-file needs a path}; shift 2 ;;
    --stop-timekpr) stop_timekpr=yes; shift ;;
    --uninstall) uninstall=yes; shift ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

fail() { echo "error: $*" >&2; exit 1; }

# The first line of a GDM dconf profile this script wrote, so --uninstall removes only its own.
KIDTIME_MARK="# written by kidtime"
GDM_PROFILE=$ROOT/etc/dconf/profile/gdm
GDM_KEYFILE=$ROOT/etc/dconf/db/gdm.d/90-kidtime

dconf_update() {
  if [ -n "$ROOT" ]; then echo "would run: dconf update"; else dconf update; fi
}

[ -n "$ROOT" ] || [ "$(id -u)" = 0 ] || fail "run this with sudo"

if [ "$stop_timekpr" = yes ]; then
  if [ -n "$ROOT" ]; then
    echo "would run: systemctl disable --now timekpr.service"
  else
    echo "== stopping timekpr"
    if systemctl cat timekpr.service >/dev/null 2>&1; then
      systemctl disable --now timekpr.service
      echo "timekpr.service: $(systemctl is-active timekpr.service || true), $(systemctl is-enabled timekpr.service || true)"
    else
      echo "timekpr.service not found; nothing to stop"
    fi
  fi
  exit 0
fi

if [ "$uninstall" = yes ]; then
  bin=$ROOT/usr/local/bin/kidtime-agent
  state=$ROOT/var/lib/kidtime/agent-state.json
  recovery() {
    cat >&2 <<'MSG'
To recover by hand (stop the agent first, or it undoes these within seconds):
  sudo systemctl disable --now kidtime-agent
  sudo usermod -U <kid>             # for each kid whose login is disabled
  sudo nft delete table inet kidtime
  sudo rm -f /etc/dconf/db/gdm.d/90-kidtime && sudo dconf update   # the login-screen banner
  # and /etc/dconf/profile/gdm, if its first line is "# written by kidtime"
MSG
  }
  if [ -n "$ROOT" ]; then
    echo "would run: systemctl stop kidtime-agent.service (so it can't re-apply blocks)"
    echo "would run: systemctl is-active --quiet kidtime-agent.service; if still active: fail, release and remove nothing"
  else
    echo "== stopping the agent"
    systemctl stop kidtime-agent.service 2>/dev/null || true
    if systemctl is-active --quiet kidtime-agent.service; then
      fail "the agent is still running; stop it and run --uninstall again"
    fi
  fi
  if [ -e "$bin" ]; then
    if [ -n "$ROOT" ]; then
      echo "would run: $bin --config $CONFIG --release-all (on failure: print recovery steps, remove nothing)"
    else
      echo "== releasing everything the agent enforced"
      if ! out=$("$bin" --config "$CONFIG" --release-all 2>&1); then
        echo "$out" >&2
        echo >&2
        echo "error: the agent could not undo everything. Nothing was removed, so this can be retried." >&2
        recovery
        exit 1
      fi
      [ -z "$out" ] || echo "$out"
    fi
  elif [ -e "$state" ]; then
    echo "error: $state exists but there is no agent binary at $bin to release it." >&2
    echo "Nothing was removed, because that file records which accounts are locked out." >&2
    recovery
    exit 1
  else
    echo "no agent binary at $bin and no state file; skipping --release-all"
  fi
  # The login-screen banner settings: the agent's file, and the profile override only if this script wrote it
  banner_files=()
  [ ! -e "$GDM_KEYFILE" ] || banner_files+=("$GDM_KEYFILE")
  if [ -f "$GDM_PROFILE" ] && grep -qxF "$KIDTIME_MARK" "$GDM_PROFILE"; then
    banner_files+=("$GDM_PROFILE")
  elif [ -e "$GDM_PROFILE" ]; then
    echo "leaving $GDM_PROFILE: kidtime didn't write it"
  fi
  if [ ${#banner_files[@]} -gt 0 ]; then
    if [ -n "$ROOT" ]; then
      echo "would remove: ${banner_files[*]}"
    else
      rm -f "${banner_files[@]}"
      echo "removed ${banner_files[*]}"
    fi
    dconf_update || echo "warning: dconf update failed" >&2
  fi
  if [ -n "$ROOT" ]; then
    echo "would run: systemctl disable --now kidtime-agent.service"
    echo "would remove: $ROOT/etc/systemd/system/kidtime-agent.service $bin $ROOT/etc/kidtime $ROOT/var/lib/kidtime"
    echo "would run: systemctl daemon-reload"
  else
    echo "== removing the agent"
    systemctl disable --now kidtime-agent.service 2>/dev/null || true
    rm -f /etc/systemd/system/kidtime-agent.service "$bin"
    rm -rf /etc/kidtime /var/lib/kidtime
    systemctl daemon-reload
    echo "removed the unit, /usr/local/bin/kidtime-agent, /etc/kidtime and /var/lib/kidtime"
  fi
  exit 0
fi

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

# Later login screens should read kidtime's banner settings (the agent writes them into gdm.d).
# The banner is optional: a failure here must not abort the install. Each step is chained with &&
# (set -e does nothing inside an if condition), so any failure takes the else branch.
stock_profile=$ROOT/usr/share/dconf/profile/gdm
if [ -e "$stock_profile" ] && [ ! -e "$GDM_PROFILE" ]; then
  tmp=
  if install -d "$ROOT/etc/dconf/profile" "$ROOT/etc/dconf/db/gdm.d" &&
    tmp=$(mktemp "$ROOT/etc/dconf/profile/.gdm.XXXXXX") &&
    { echo "$KIDTIME_MARK"; echo "user-db:user"; echo "system-db:gdm"; grep -v '^user-db:' "$stock_profile" || true; } > "$tmp" &&
    chmod 644 "$tmp" &&
    mv "$tmp" "$GDM_PROFILE" &&
    dconf_update; then
    echo "enabled login-screen banner settings (/etc/dconf/profile/gdm)"
  else
    [ -z "$tmp" ] || rm -f "$tmp"
    rm -f "$GDM_PROFILE"
    echo "warning: could not enable the login-screen banner settings; continuing without them" >&2
  fi
fi

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
