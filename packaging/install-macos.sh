#!/usr/bin/env bash
#
# Interactive installer for a scheduled rlocate index rebuild on macOS (launchd).
# Run as root:
#
#   sudo ./packaging/install-macos.sh
#
# It asks for the paths to index, the rebuild interval, and the database path,
# then builds/installs the binaries and installs + loads a LaunchDaemon that
# rebuilds the index on that interval (and once at boot).

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

# --------------------------------------------------------------------------
# preflight
# --------------------------------------------------------------------------
[[ $(uname) == Darwin ]] || die "this installer is for macOS; use install.sh on Linux"
[[ $EUID -eq 0 ]] || die "must run as root (try: sudo $0)"

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" >/dev/null && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." >/dev/null && pwd)
LABEL="com.rlocate.updatedb"
PLIST="/Library/LaunchDaemons/${LABEL}.plist"

printf '%s\n\n' "${BOLD}rlocate scheduled-index installer (macOS)${RESET}"

# --------------------------------------------------------------------------
# binaries
# --------------------------------------------------------------------------
ask BINDIR "Install binaries to" "/usr/local/bin"
RELEASE_DIR="$REPO_ROOT/target/release"
if [[ -x "$RELEASE_DIR/rlocate-updatedb" ]]; then
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
# questions
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
# confirm
# --------------------------------------------------------------------------
echo
printf '%s\n' "${BOLD}Summary${RESET}"
cat <<EOF
  binary     -> ${BINDIR}/rlocate-updatedb
  database   -> ${DB_PATH}
  index roots-> ${ROOTS} ${ONE_FS_FLAG}
  interval   -> every ${INTERVAL_MIN} min (${INTERVAL_SEC}s) + at boot
  daemon     -> ${PLIST}
  log        -> /var/log/rlocate-updatedb.log
EOF
echo
ask_yn "Proceed with installation?" y || { info "aborted."; exit 0; }

# --------------------------------------------------------------------------
# install
# --------------------------------------------------------------------------
install -m 0755 "$RELEASE_DIR/rlocate-updatedb" "$BINDIR/rlocate-updatedb"
[[ -x "$RELEASE_DIR/rlocate" ]] && install -m 0755 "$RELEASE_DIR/rlocate" "$BINDIR/rlocate"
install -d -m 0755 "$(dirname "$DB_PATH")"

# Build the ProgramArguments array (updatedb --output DB [--one-file-system] ROOTS).
prog_args=$(
  printf '        <string>%s</string>\n' "$BINDIR/rlocate-updatedb"
  printf '        <string>--output</string>\n        <string>%s</string>\n' "$DB_PATH"
  [[ -n "$ONE_FS_FLAG" ]] && printf '        <string>%s</string>\n' "$ONE_FS_FLAG"
  for r in $ROOTS; do printf '        <string>%s</string>\n' "$r"; done
)

cat > "$PLIST" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>${LABEL}</string>
    <key>ProgramArguments</key>
    <array>
${prog_args}    </array>
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
chown root:wheel "$PLIST"
chmod 644 "$PLIST"

# (Re)load the daemon. RunAtLoad triggers the first rebuild immediately.
launchctl bootout system "$PLIST" 2>/dev/null || true
launchctl bootstrap system "$PLIST"
launchctl enable "system/${LABEL}" 2>/dev/null || true

echo
ok "Installed and loaded ${LABEL}."
echo
warn "ACTION REQUIRED — grant Full Disk Access, or protected folders are skipped:"
cat <<EOF
  System Settings > Privacy & Security > Full Disk Access > +
  add:  ${BINDIR}/rlocate-updatedb
  (then the next hourly run, or 'sudo launchctl kickstart -k system/${LABEL}',
   will index Desktop/Documents/Downloads/Mail/etc.)

Verify:
  sudo launchctl kickstart -k system/${LABEL}   # run a rebuild now
  tail -f /var/log/rlocate-updatedb.log
  rlocate -d ${DB_PATH} -S                       # path count
  launchctl print system/${LABEL} | head         # daemon state

Uninstall:
  sudo launchctl bootout system ${PLIST}
  sudo rm ${PLIST}
EOF
