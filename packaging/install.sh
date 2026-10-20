#!/usr/bin/env bash
#
# Interactive installer for the rlocate server + hourly (or custom) index
# rebuild on a systemd Linux host. Run as root:
#
#   sudo ./packaging/install.sh
#
# It asks for the shared secret, the paths to index, the rebuild frequency, and
# the listen address, then builds/installs the binaries, stores the token, and
# writes + enables the systemd units.

set -euo pipefail

# --------------------------------------------------------------------------
# helpers
# --------------------------------------------------------------------------
BOLD=$'\033[1m'; DIM=$'\033[2m'; RED=$'\033[31m'; GREEN=$'\033[32m'; RESET=$'\033[0m'
info()  { printf '%s\n' "${DIM}$*${RESET}"; }
ok()    { printf '%s\n' "${GREEN}$*${RESET}"; }
err()   { printf '%s\n' "${RED}$*${RESET}" >&2; }
die()   { err "error: $*"; exit 1; }

ask() { # ask VAR "prompt" "default"
  local __var=$1 prompt=$2 default=${3-} reply
  if [[ -n "$default" ]]; then
    read -rp "${BOLD}${prompt}${RESET} [${default}]: " reply || true
    reply=${reply:-$default}
  else
    read -rp "${BOLD}${prompt}${RESET}: " reply || true
  fi
  printf -v "$__var" '%s' "$reply"
}

ask_yn() { # ask_yn "prompt" default(y/n) -> returns 0 for yes
  local prompt=$1 default=${2:-y} reply hint
  [[ $default == y ]] && hint="Y/n" || hint="y/N"
  read -rp "${BOLD}${prompt}${RESET} [${hint}]: " reply || true
  reply=${reply:-$default}
  [[ $reply =~ ^[Yy] ]]
}

gen_token() {
  if command -v openssl >/dev/null 2>&1; then
    openssl rand -hex 24
  else
    head -c 24 /dev/urandom | base64 | tr -d '/+=' | cut -c1-32
  fi
}

# --------------------------------------------------------------------------
# preflight
# --------------------------------------------------------------------------
[[ $EUID -eq 0 ]] || die "must run as root (try: sudo $0)"
command -v systemctl >/dev/null 2>&1 || die "systemd (systemctl) not found; this installer is for systemd hosts"

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" >/dev/null && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." >/dev/null && pwd)

printf '%s\n\n' "${BOLD}rlocate server installer${RESET}"

# --------------------------------------------------------------------------
# binaries: use prebuilt, or build with cargo
# --------------------------------------------------------------------------
ask BINDIR "Install binaries to" "/usr/local/bin"
RELEASE_DIR="$REPO_ROOT/target/release"
if [[ -x "$RELEASE_DIR/rlocate" && -x "$RELEASE_DIR/rlocate-updatedb" ]]; then
  info "Found prebuilt binaries in target/release."
elif command -v cargo >/dev/null 2>&1; then
  if ask_yn "Binaries not built yet. Build them now with cargo?" y; then
    info "Building (release)…"
    ( cd "$REPO_ROOT" && cargo build --release )
  else
    die "no binaries to install"
  fi
else
  die "no prebuilt binaries and cargo not found. Install Rust (https://rustup.rs) and retry, or build elsewhere and copy target/release/ here."
fi

# --------------------------------------------------------------------------
# questions
# --------------------------------------------------------------------------
echo
DB_PATH=/var/lib/rlocate/rlocate.db
ask DB_PATH "Index database path" "$DB_PATH"

ask ROOTS "Paths to index (space-separated)" "/"

ONE_FS_FLAG=""
if ask_yn "Stay on one filesystem (don't descend into other mounts)?" n; then
  ONE_FS_FLAG="--one-file-system"
fi

ask EXTRA_PRUNE "Extra directories to skip (space-separated, blank for none)" ""

echo
info "Rebuild frequency (systemd OnCalendar):"
info "  1) hourly   2) every 6 hours   3) daily (03:00)   4) custom"
ask FREQ_CHOICE "Choose 1-4" "1"
case "$FREQ_CHOICE" in
  1) ONCALENDAR="hourly" ;;
  2) ONCALENDAR="*-*-* 00/6:00:00" ;;
  3) ONCALENDAR="*-*-* 03:00:00" ;;
  4)
    while :; do
      ask ONCALENDAR "Enter OnCalendar expression" "daily"
      if command -v systemd-analyze >/dev/null 2>&1; then
        if systemd-analyze calendar "$ONCALENDAR" >/dev/null 2>&1; then break; fi
        err "invalid OnCalendar expression; try again (e.g. 'daily', '*-*-* 02:00:00')"
      else
        break
      fi
    done ;;
  *) ONCALENDAR="hourly" ;;
esac

echo
ask LISTEN "Listen address (host:port)" "0.0.0.0:9999"
if [[ "$LISTEN" == 0.0.0.0:* ]]; then
  info "Note: 0.0.0.0 exposes the server on all interfaces. Firewall the port or use 127.0.0.1 + an SSH tunnel if the host is not on a trusted network."
fi

# --------------------------------------------------------------------------
# token
# --------------------------------------------------------------------------
echo
TOKEN=""
if ask_yn "Auto-generate the shared secret token?" y; then
  TOKEN=$(gen_token)
  ok "Generated token: ${TOKEN}"
else
  while [[ -z "$TOKEN" ]]; do
    read -rsp "${BOLD}Enter token${RESET}: " TOKEN; echo
    [[ -n "$TOKEN" ]] || err "token must not be empty"
  done
fi

USE_CREDS=false
if command -v systemd-creds >/dev/null 2>&1; then
  if ask_yn "Store the token as an encrypted systemd credential (recommended)?" y; then
    USE_CREDS=true
  fi
else
  info "systemd-creds not available; falling back to a root-only plaintext token file."
fi

# --------------------------------------------------------------------------
# confirm
# --------------------------------------------------------------------------
echo
printf '%s\n' "${BOLD}Summary${RESET}"
cat <<EOF
  binaries   -> ${BINDIR}/rlocate, ${BINDIR}/rlocate-updatedb
  database   -> ${DB_PATH}
  index roots-> ${ROOTS} ${ONE_FS_FLAG}
  extra skip -> ${EXTRA_PRUNE:-(none)}
  rebuild    -> OnCalendar=${ONCALENDAR}
  listen     -> ${LISTEN}
  token      -> $([[ $USE_CREDS == true ]] && echo "encrypted (/etc/rlocate/token.cred)" || echo "plaintext 0600 (/etc/rlocate/token)")
EOF
echo
ask_yn "Proceed with installation?" y || { info "aborted."; exit 0; }

# --------------------------------------------------------------------------
# install
# --------------------------------------------------------------------------
install -m 0755 "$RELEASE_DIR/rlocate"          "$BINDIR/rlocate"
install -m 0755 "$RELEASE_DIR/rlocate-updatedb" "$BINDIR/rlocate-updatedb"
install -d -m 0755 "$(dirname "$DB_PATH")"
install -d -m 0755 /etc/rlocate

# token storage + the server's token argument
if [[ $USE_CREDS == true ]]; then
  printf '%s' "$TOKEN" | systemd-creds encrypt --name=token - /etc/rlocate/token.cred
  chmod 600 /etc/rlocate/token.cred
  CRED_LINE="LoadCredentialEncrypted=token:/etc/rlocate/token.cred"
  TOKEN_ARG="--token-file %d/token"
else
  printf '%s\n' "$TOKEN" > /etc/rlocate/token
  chmod 600 /etc/rlocate/token
  CRED_LINE=""
  TOKEN_ARG="--token-file /etc/rlocate/token"
fi

# optional updatedb.conf for extra prune paths
CONF_ARG=""
if [[ -n "${EXTRA_PRUNE// }" ]]; then
  printf 'PRUNEPATHS = "%s"\n' "$EXTRA_PRUNE" > /etc/rlocate/updatedb.conf
  chmod 644 /etc/rlocate/updatedb.conf
  CONF_ARG="--conf /etc/rlocate/updatedb.conf"
fi

# server unit
{
  echo "[Unit]"
  echo "Description=rlocate query server"
  echo "After=network-online.target"
  echo "Wants=network-online.target"
  echo "ConditionPathExists=${DB_PATH}"
  echo
  echo "[Service]"
  echo "Type=simple"
  [[ -n "$CRED_LINE" ]] && echo "$CRED_LINE"
  echo "ExecStart=${BINDIR}/rlocate --serve --listen ${LISTEN} --database ${DB_PATH} ${TOKEN_ARG}"
  echo "Restart=on-failure"
  echo "RestartSec=2"
  echo "NoNewPrivileges=yes"
  echo "ProtectSystem=strict"
  echo "ProtectHome=read-only"
  echo "PrivateTmp=yes"
  echo
  echo "[Install]"
  echo "WantedBy=multi-user.target"
} > /etc/systemd/system/rlocate-server.service

# updatedb oneshot
{
  echo "[Unit]"
  echo "Description=Rebuild the rlocate index"
  echo
  echo "[Service]"
  echo "Type=oneshot"
  echo "ExecStart=${BINDIR}/rlocate-updatedb --output ${DB_PATH} ${CONF_ARG} ${ONE_FS_FLAG} ${ROOTS}"
  echo "ExecStartPost=-/usr/bin/systemctl try-restart rlocate-server.service"
  echo "Nice=10"
  echo "IOSchedulingClass=idle"
} > /etc/systemd/system/rlocate-updatedb.service

# updatedb timer
{
  echo "[Unit]"
  echo "Description=Rebuild the rlocate index on a schedule"
  echo
  echo "[Timer]"
  echo "OnCalendar=${ONCALENDAR}"
  echo "Persistent=true"
  echo "RandomizedDelaySec=5min"
  echo
  echo "[Install]"
  echo "WantedBy=timers.target"
} > /etc/systemd/system/rlocate-updatedb.timer

systemctl daemon-reload

# --------------------------------------------------------------------------
# first build + enable
# --------------------------------------------------------------------------
echo
info "Building the initial index (this can take a while for large trees)…"
systemctl start rlocate-updatedb.service
"$BINDIR/rlocate" -d "$DB_PATH" -S || true

systemctl enable --now rlocate-server.service
systemctl enable --now rlocate-updatedb.timer

echo
ok "Installed and running."
echo
cat <<EOF
${BOLD}Client setup${RESET} — add this to ~/.config/rlocate/remotes.conf on each client
(chmod 600 it), using this host's address:

  [${HOSTNAME:-this-host}]
  host = <this-host-ip>:${LISTEN##*:}
  token = ${TOKEN}

Useful commands:
  systemctl status rlocate-server.service
  systemctl list-timers rlocate-updatedb.timer
  journalctl -u rlocate-server.service -e
EOF
