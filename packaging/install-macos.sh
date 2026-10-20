#!/usr/bin/env bash
#
# Interactive installer for a scheduled rlocate index rebuild on macOS (launchd),
# with an optional query server. Run as root:
#
#   sudo ./packaging/install-macos.sh
#
# It asks for the paths to index, the rebuild interval, and the database path
# (and, if you opt in, a listen address + token for the server), then builds and
# installs the binaries and loads the LaunchDaemon(s).

set -euo pipefail

BOLD=$'\033[1m'; DIM=$'\033[2m'; RED=$'\033[31m'; GREEN=$'\033[32m'; YELLOW=$'\033[33m'; RESET=$'\033[0m'
info()  { printf '%s\n' "${DIM}$*${RESET}"; }
ok()    { printf '%s\n' "${GREEN}$*${RESET}"; }
warn()  { printf '%s\n' "${YELLOW}$*${RESET}"; }
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

ask_yn() { # ask_yn "prompt" default(y/n)
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
[[ $(uname) == Darwin ]] || die "this installer is for macOS; use install.sh on Linux"
[[ $EUID -eq 0 ]] || die "must run as root (try: sudo $0)"

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" >/dev/null && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." >/dev/null && pwd)

SCAN_LABEL="com.rlocate.updatedb"
SCAN_PLIST="/Library/LaunchDaemons/${SCAN_LABEL}.plist"
SRV_LABEL="com.rlocate.server"
SRV_PLIST="/Library/LaunchDaemons/${SRV_LABEL}.plist"
REFRESH="/usr/local/bin/rlocate-refresh"
TOKEN_FILE="/usr/local/etc/rlocate/token"

printf '%s\n\n' "${BOLD}rlocate installer (macOS)${RESET}"

# --------------------------------------------------------------------------
# binaries
# --------------------------------------------------------------------------
ask BINDIR "Install binaries to" "/usr/local/bin"
RELEASE_DIR="$REPO_ROOT/target/release"
if [[ -x "$RELEASE_DIR/rlocate-updatedb" && -x "$RELEASE_DIR/rlocate" ]]; then
  info "Found prebuilt binaries in target/release."
elif command -v cargo >/dev/null 2>&1; then
  if ask_yn "Binaries not built yet. Build them now with cargo?" y; then
    ( cd "$REPO_ROOT" && cargo build --release )
  else
    die "no binaries to install"
  fi
else
  die "no prebuilt binaries and cargo not found. Install Rust (https://rustup.rs) and retry."
fi

# --------------------------------------------------------------------------
# questions: scanning
# --------------------------------------------------------------------------
echo
DB_PATH=/var/db/rlocate/rlocate.db
ask DB_PATH "Index database path" "$DB_PATH"
ask ROOTS "Paths to index (space-separated)" "/"

ONE_FS_FLAG=""
if ask_yn "Stay on one filesystem (don't descend into other mounts)?" n; then
  ONE_FS_FLAG="--one-file-system"
fi

ask INTERVAL_MIN "Rebuild every how many minutes?" "60"
[[ "$INTERVAL_MIN" =~ ^[0-9]+$ && "$INTERVAL_MIN" -gt 0 ]] || die "interval must be a positive integer"
INTERVAL_SEC=$(( INTERVAL_MIN * 60 ))

# --------------------------------------------------------------------------
# questions: optional server
# --------------------------------------------------------------------------
echo
SERVER=false
LISTEN=""; TOKEN=""
if ask_yn "Also run the query server so other hosts can search this Mac?" n; then
  SERVER=true
  ask LISTEN "Listen address (host:port)" "0.0.0.0:9999"
  [[ "$LISTEN" == 0.0.0.0:* ]] && info "Note: 0.0.0.0 exposes the server on all interfaces; firewall it or use 127.0.0.1 + an SSH tunnel off a trusted network."
  if ask_yn "Auto-generate the shared secret token?" y; then
    TOKEN=$(gen_token); ok "Generated token: ${TOKEN}"
  else
    while [[ -z "$TOKEN" ]]; do read -rsp "${BOLD}Enter token${RESET}: " TOKEN; echo; [[ -n "$TOKEN" ]] || err "token must not be empty"; done
  fi
fi

# --------------------------------------------------------------------------
# confirm
# --------------------------------------------------------------------------
echo
printf '%s\n' "${BOLD}Summary${RESET}"
cat <<EOF
  binaries   -> ${BINDIR}/rlocate-updatedb$( $SERVER && printf ', %s/rlocate' "$BINDIR" )
  database   -> ${DB_PATH}
  index roots-> ${ROOTS} ${ONE_FS_FLAG}
  interval   -> every ${INTERVAL_MIN} min + at boot
  server     -> $( $SERVER && echo "yes, listening on ${LISTEN} (token in ${TOKEN_FILE}, 0600)" || echo "no" )
EOF
echo
ask_yn "Proceed with installation?" y || { info "aborted."; exit 0; }

# --------------------------------------------------------------------------
# install binaries + dirs
# --------------------------------------------------------------------------
install -m 0755 "$RELEASE_DIR/rlocate-updatedb" "$BINDIR/rlocate-updatedb"
install -m 0755 "$RELEASE_DIR/rlocate"          "$BINDIR/rlocate"
install -d -m 0755 "$(dirname "$DB_PATH")"

# --------------------------------------------------------------------------
# optional server: token file, refresh wrapper, server plist
# --------------------------------------------------------------------------
if $SERVER; then
  install -d -m 0755 "$(dirname "$TOKEN_FILE")"
  printf '%s\n' "$TOKEN" > "$TOKEN_FILE"
  chown root:wheel "$TOKEN_FILE"; chmod 600 "$TOKEN_FILE"

  # The server mmaps the index at startup, so restart it after each rebuild.
  cat > "$REFRESH" <<EOF
#!/bin/sh
# Rebuild the index, then restart the server so it re-opens the new index.
"${BINDIR}/rlocate-updatedb" --output "${DB_PATH}" ${ONE_FS_FLAG} ${ROOTS} \\
  && /bin/launchctl kickstart -k "system/${SRV_LABEL}"
EOF
  chmod 755 "$REFRESH"

  cat > "$SRV_PLIST" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>${SRV_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>${BINDIR}/rlocate</string>
        <string>--serve</string>
        <string>--listen</string>
        <string>${LISTEN}</string>
        <string>--database</string>
        <string>${DB_PATH}</string>
        <string>--token-file</string>
        <string>${TOKEN_FILE}</string>
    </array>
    <key>KeepAlive</key>
    <true/>
    <key>RunAtLoad</key>
    <true/>
    <key>StandardOutPath</key>
    <string>/var/log/rlocate-server.log</string>
    <key>StandardErrorPath</key>
    <string>/var/log/rlocate-server.log</string>
</dict>
</plist>
EOF
  chown root:wheel "$SRV_PLIST"; chmod 644 "$SRV_PLIST"
fi

# --------------------------------------------------------------------------
# scan daemon plist
# --------------------------------------------------------------------------
if $SERVER; then
  # Run via the refresh wrapper so each rebuild also restarts the server.
  scan_prog=$(printf '        <string>/bin/sh</string>\n        <string>%s</string>\n' "$REFRESH")
else
  scan_prog=$(
    printf '        <string>%s</string>\n' "$BINDIR/rlocate-updatedb"
    printf '        <string>--output</string>\n        <string>%s</string>\n' "$DB_PATH"
    [[ -n "$ONE_FS_FLAG" ]] && printf '        <string>%s</string>\n' "$ONE_FS_FLAG"
    for r in $ROOTS; do printf '        <string>%s</string>\n' "$r"; done
  )
fi

cat > "$SCAN_PLIST" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>${SCAN_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
${scan_prog}    </array>
    <key>StartInterval</key>
    <integer>${INTERVAL_SEC}</integer>
    <key>RunAtLoad</key>
    <true/>
    <key>StandardOutPath</key>
    <string>/var/log/rlocate-updatedb.log</string>
    <key>StandardErrorPath</key>
    <string>/var/log/rlocate-updatedb.log</string>
    <key>LowPriorityIO</key>
    <true/>
    <key>Nice</key>
    <integer>10</integer>
    <key>ProcessType</key>
    <string>Background</string>
</dict>
</plist>
EOF
chown root:wheel "$SCAN_PLIST"; chmod 644 "$SCAN_PLIST"

# --------------------------------------------------------------------------
# build the initial index (foreground), then load the daemon(s)
# --------------------------------------------------------------------------
echo
info "Building the initial index (this can take a while for large trees)…"
# shellcheck disable=SC2086
"$BINDIR/rlocate-updatedb" --output "$DB_PATH" $ONE_FS_FLAG $ROOTS || \
  warn "initial build reported an error (often missing Full Disk Access — see below)"

launchctl bootout system "$SCAN_PLIST" 2>/dev/null || true
launchctl bootstrap system "$SCAN_PLIST"
launchctl enable "system/${SCAN_LABEL}" 2>/dev/null || true

if $SERVER; then
  launchctl bootout system "$SRV_PLIST" 2>/dev/null || true
  launchctl bootstrap system "$SRV_PLIST"
  launchctl enable "system/${SRV_LABEL}" 2>/dev/null || true
fi

# --------------------------------------------------------------------------
# done
# --------------------------------------------------------------------------
echo
ok "Installed."
"$BINDIR/rlocate" -d "$DB_PATH" -S || true
echo
warn "ACTION REQUIRED — grant Full Disk Access or protected folders are skipped:"
cat <<EOF
  System Settings > Privacy & Security > Full Disk Access > +
  add:  ${BINDIR}/rlocate-updatedb
  then: sudo launchctl kickstart -k system/${SCAN_LABEL}

Logs:   tail -f /var/log/rlocate-updatedb.log
Status: launchctl print system/${SCAN_LABEL} | head
EOF

if $SERVER; then
  echo
  cat <<EOF
${BOLD}Server running on ${LISTEN}.${RESET} On each client add to
~/.config/rlocate/remotes.conf (chmod 600):

  [$(scutil --get LocalHostName 2>/dev/null || echo this-mac)]
  host = <this-mac-ip>:${LISTEN##*:}
  token = ${TOKEN}

Server logs:  tail -f /var/log/rlocate-server.log
EOF
fi
