#!/usr/bin/env bash
# hwstats installer for Ubuntu 22.04/24.04 and RHEL-family 8/9/10
# (RHEL, Rocky, AlmaLinux, CentOS Stream, Oracle Linux).
#
#   ./install.sh               build and install, or upgrade an existing install
#   ./install.sh --build-only  only build target/release/hwstats
#   ./install.sh --rollback    go back to the binary installed before the last upgrade
#   ./install.sh --uninstall   remove the service, keep data and config
#   ./install.sh --purge       remove everything, including history and backups
#
# Run it as your normal user (sudo is used where needed) or as root.

set -Eeuo pipefail

APP=hwstats
BIN=/usr/local/bin/$APP
PREV=$BIN.previous
CONF_DIR=/etc/$APP
CONF=$CONF_DIR/config.toml
UNIT=/etc/systemd/system/$APP.service
STATE_DIR=/var/lib/$APP
DOC_DIR=/usr/local/share/doc/$APP
SRC_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MODE=install
UPGRADE=0
ASSUME_YES=0
STEP="start"
HINT=""

usage() {
  sed -n '2,11p' "$0" | sed 's/^# \{0,1\}//'
  echo "Options: --build-only --rollback --uninstall --purge [--yes] --help"
}

for arg in "$@"; do
  case "$arg" in
    --build-only) MODE=build ;;
    --rollback) MODE=rollback ;;
    --uninstall) MODE=uninstall ;;
    --purge) MODE=purge ;;
    --yes|-y) ASSUME_YES=1 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown option: $arg" >&2; usage; exit 2 ;;
  esac
done

if [ -t 1 ]; then BOLD=$'\033[1m'; DIM=$'\033[2m'; RESET=$'\033[0m'; else BOLD=""; DIM=""; RESET=""; fi
say()  { printf '%s==>%s %s\n' "$BOLD" "$RESET" "$*"; }
info() { printf '    %s%s%s\n' "$DIM" "$*" "$RESET"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die()  { printf 'error: %s\n' "$*" >&2; exit 1; }
step() { STEP="$1"; HINT="${2:-}"; say "$1"; }

on_error() {
  local code=$?
  printf '\nerror: step "%s" failed (exit code %s).\n' "$STEP" "$code" >&2
  [ -n "$HINT" ] && printf 'hint: %s\n' "$HINT" >&2
  exit "$code"
}
trap on_error ERR

# ---------------------------------------------------------------- privileges

if [ "$(id -u)" -eq 0 ]; then
  SUDO=""
else
  command -v sudo >/dev/null 2>&1 || die "run this script as root or install sudo"
  SUDO="sudo"
fi

if [ "$(id -u)" -eq 0 ] && [ -n "${SUDO_USER:-}" ] && [ "${SUDO_USER}" != "root" ]; then
  BUILD_USER="$SUDO_USER"
else
  BUILD_USER="$(id -un)"
fi
BUILD_HOME="$(getent passwd "$BUILD_USER" | cut -d: -f6)"
[ -n "$BUILD_HOME" ] || die "cannot find the home directory of $BUILD_USER"

# Run a command as the (non-root) build user.
as_builder() {
  if [ "$(id -un)" = "$BUILD_USER" ]; then
    "$@"
  else
    runuser -u "$BUILD_USER" -- "$@"
  fi
}

have_systemd() { [ -d /run/systemd/system ] && command -v systemctl >/dev/null 2>&1; }

# Run a command as the service user.
as_service_user() {
  if [ "$(id -u)" -eq 0 ]; then
    runuser -u "$APP" -- "$@"
  else
    sudo -u "$APP" "$@"
  fi
}

detect_install() {
  if [ -x "$BIN" ] || [ -f "$UNIT" ]; then
    UPGRADE=1
  fi
}

# ---------------------------------------------------------------- os detection

OS_ID=""; OS_LIKE=""; OS_VER=""; PKG=""
detect_os() {
  [ -r /etc/os-release ] || die "/etc/os-release not found; unsupported system"
  # shellcheck disable=SC1091
  OS_ID="$(. /etc/os-release && echo "${ID:-}")"
  OS_LIKE="$(. /etc/os-release && echo "${ID_LIKE:-}")"
  OS_VER="$(. /etc/os-release && echo "${VERSION_ID:-}")"
  local major="${OS_VER%%.*}"
  case " $OS_ID $OS_LIKE " in
    *" ubuntu "*|*" debian "*) PKG=apt ;;
    *" rhel "*|*" fedora "*|*" centos "*) PKG=dnf; command -v dnf >/dev/null 2>&1 || PKG=yum ;;
    *) die "unsupported distribution '$OS_ID' (supported: Ubuntu 22.04/24.04, RHEL-family 8/9/10)" ;;
  esac
  local supported=0
  case "$OS_ID" in
    ubuntu) case "$OS_VER" in 22.04|24.04) supported=1 ;; esac ;;
    rhel|rocky|almalinux|centos|ol) case "$major" in 8|9|10) supported=1 ;; esac ;;
  esac
  if [ "$supported" -eq 1 ]; then
    info "detected $OS_ID $OS_VER (package manager: $PKG)"
  else
    warn "$OS_ID $OS_VER is not one of the tested systems; trying the $PKG path anyway"
  fi
}

# ---------------------------------------------------------------- preflight

config_port() {
  local f="$CONF"
  [ -f "$f" ] || f="$SRC_DIR/packaging/config.toml"
  local p
  p="$(grep -E '^[[:space:]]*listen[[:space:]]*=' "$f" | head -n1 | sed -E 's/.*:([0-9]+)".*/\1/')"
  echo "${p:-9393}"
}

free_kb() { df -Pk "$1" 2>/dev/null | awk 'NR==2 {print $4}'; }

preflight() {
  step "Checking the system"
  case "$(uname -m)" in
    x86_64|aarch64) ;;
    *) warn "architecture $(uname -m) is untested" ;;
  esac
  local kb
  kb="$(free_kb "$SRC_DIR")"
  if [ -n "$kb" ] && [ "$kb" -lt 2097152 ]; then
    die "the build needs about 2 GB free in $SRC_DIR (only $((kb / 1024)) MB free)"
  fi
  if [ "$MODE" = install ]; then
    kb="$(free_kb /var/lib)"
    if [ -n "$kb" ] && [ "$kb" -lt 512000 ]; then
      die "/var/lib needs at least 500 MB free for history and backups (only $((kb / 1024)) MB free)"
    fi
    if ! have_systemd; then
      warn "systemd is not running (container or WSL?): the binary will be installed but not started as a service"
    fi
    local port
    port="$(config_port)"
    if command -v ss >/dev/null 2>&1 && ss -ltnH "( sport = :$port )" 2>/dev/null | grep -q .; then
      if have_systemd && systemctl is-active --quiet "$APP" 2>/dev/null; then
        info "port $port is used by the running $APP service (it will be upgraded)"
      else
        die "port $port is already in use by another program; change 'listen' in $CONF (or packaging/config.toml before the first install)"
      fi
    fi
  fi
}

# ---------------------------------------------------------------- packages

install_packages() {
  step "Installing build dependencies" "check that the package repositories are reachable"
  local need=()
  case "$PKG" in
    apt)
      command -v cc >/dev/null 2>&1 || need+=(build-essential)
      command -v curl >/dev/null 2>&1 || need+=(curl)
      [ -e /etc/ssl/certs/ca-certificates.crt ] || need+=(ca-certificates)
      if [ "${#need[@]}" -gt 0 ]; then
        info "installing: ${need[*]}"
        $SUDO apt-get update -qq
        $SUDO env DEBIAN_FRONTEND=noninteractive apt-get install -y -qq "${need[@]}"
      fi
      ;;
    dnf|yum)
      command -v cc >/dev/null 2>&1 || need+=(gcc)
      # RHEL 9/10 ship curl-minimal, which conflicts with the full curl package.
      command -v curl >/dev/null 2>&1 || need+=(curl)
      [ -e /etc/pki/tls/certs/ca-bundle.crt ] || need+=(ca-certificates)
      if [ "${#need[@]}" -gt 0 ]; then
        if [ -z "$($SUDO "$PKG" -q repolist --enabled 2>/dev/null | tail -n +2)" ]; then
          warn "no enabled package repositories were detected; if the installation below fails, register the system (sudo subscription-manager register) or enable the BaseOS/AppStream repositories"
        fi
        info "installing: ${need[*]}"
        $SUDO "$PKG" install -y -q "${need[@]}"
      fi
      ;;
  esac
  [ "${#need[@]}" -eq 0 ] && info "everything needed is already installed"
  return 0
}

# ---------------------------------------------------------------- rust

CARGO=""
version_ge() { [ "$(printf '%s\n%s\n' "$2" "$1" | sort -V | head -n1)" = "$2" ]; }

setup_rust() {
  step "Checking the Rust toolchain" "the Rust installer needs internet access to static.rust-lang.org"
  local req
  req="$(grep -E '^rust-version' "$SRC_DIR/Cargo.toml" | cut -d'"' -f2)"
  local candidates=("$BUILD_HOME/.cargo/bin/cargo")
  local sys
  sys="$(as_builder bash -c 'command -v cargo' 2>/dev/null || true)"
  [ -n "$sys" ] && candidates+=("$sys")
  for c in "${candidates[@]}"; do
    if [ -x "$c" ]; then
      local v
      v="$(as_builder "$c" --version 2>/dev/null | awk '{print $2}')"
      if [ -n "$v" ] && version_ge "$v" "$req"; then
        CARGO="$c"
        info "using $c ($v, needs $req or newer)"
        return 0
      fi
      info "$c is version ${v:-unknown}; $req or newer is required"
    fi
  done

  if [ -x "$BUILD_HOME/.cargo/bin/rustup" ]; then
    info "updating the stable toolchain with rustup"
    as_builder "$BUILD_HOME/.cargo/bin/rustup" update stable --no-self-update
    as_builder "$BUILD_HOME/.cargo/bin/rustup" default stable
  else
    curl -fsS --max-time 15 -o /dev/null https://static.rust-lang.org/rustup/release-stable.toml \
      || die "cannot reach static.rust-lang.org to download Rust; check the internet connection or proxy settings"
    local tmp="${TMPDIR:-/tmp}"
    if command -v findmnt >/dev/null 2>&1 && findmnt -no OPTIONS --target "$tmp" 2>/dev/null | grep -qw noexec; then
      tmp="$BUILD_HOME/.cache/hwstats-build"
      as_builder mkdir -p "$tmp"
      info "$tmp is used for temporary files because /tmp is mounted noexec"
    fi
    local script
    script="$(as_builder mktemp "$tmp/rustup-init.XXXXXX")"
    as_builder curl --proto '=https' --tlsv1.2 -fsS https://sh.rustup.rs -o "$script"
    info "installing Rust (stable, minimal profile) for $BUILD_USER in $BUILD_HOME/.cargo"
    as_builder env TMPDIR="$tmp" RUSTUP_INIT_SKIP_PATH_CHECK=yes \
      sh "$script" -y --profile minimal --default-toolchain stable --no-modify-path
    as_builder rm -f "$script"
  fi
  CARGO="$BUILD_HOME/.cargo/bin/cargo"
  [ -x "$CARGO" ] || die "cargo was not installed at $CARGO"
  info "using $CARGO ($(as_builder "$CARGO" --version))"
}

# ---------------------------------------------------------------- build

build() {
  step "Building hwstats (release)" "check internet access to crates.io and free disk space; then run: $CARGO build --release --locked"
  if ! as_builder test -w "$SRC_DIR"; then
    die "$BUILD_USER cannot write to $SRC_DIR; run the script from a copy owned by that user"
  fi
  curl -fsS --max-time 15 -o /dev/null https://index.crates.io/config.json \
    || warn "crates.io is not reachable; the build only works if all crates are already downloaded"
  local cargo_dir
  cargo_dir="$(dirname "$CARGO")"
  (cd "$SRC_DIR" && as_builder env PATH="$cargo_dir:$PATH" "$CARGO" build --release --locked)
  [ -x "$SRC_DIR/target/release/$APP" ] || die "build finished but target/release/$APP is missing"
  info "built $SRC_DIR/target/release/$APP"
}

# ---------------------------------------------------------------- upgrade

prepare_upgrade() {
  [ "$UPGRADE" -eq 1 ] || return 0
  step "Preparing the upgrade" "the installed version was not changed yet"
  local old new
  old="$("$BIN" --version 2>/dev/null | awk '{print $2}' || true)"
  new="$(grep -E '^version' "$SRC_DIR/Cargo.toml" | head -n1 | cut -d'"' -f2)"
  info "upgrading hwstats ${old:-unknown} to $new"
  info "kept: history and backups ($STATE_DIR), settings (stored in the database) and $CONF"
  if [ -x "$BIN" ] && cmp -s "$SRC_DIR/target/release/$APP" "$BIN"; then
    info "the new binary is identical to the installed one"
  fi
  if [ -x "$BIN" ] && getent passwd "$APP" >/dev/null; then
    info "backing up the database before the upgrade"
    local out
    if out="$(as_service_user "$BIN" --config "$CONF" backup 2>&1)"; then
      while IFS= read -r line; do info "$line"; done <<<"$out"
    else
      warn "the pre-upgrade backup failed (${out##*$'\n'}); continuing, a schema change takes its own backup"
    fi
  fi
  if [ -x "$BIN" ]; then
    $SUDO install -m 0755 "$BIN" "$PREV"
    if command -v restorecon >/dev/null 2>&1; then $SUDO restorecon -F "$PREV" || true; fi
    info "previous binary kept as $PREV (./install.sh --rollback returns to it)"
  fi
}

rollback() {
  step "Rolling back to the previous version" "see the log with: journalctl -u $APP -n 100"
  [ -x "$PREV" ] || die "there is no previous version at $PREV"
  $SUDO install -m 0755 "$PREV" "$BIN"
  if command -v restorecon >/dev/null 2>&1; then $SUDO restorecon -F "$BIN" || true; fi
  info "restored $BIN from $PREV"
  if have_systemd; then
    $SUDO systemctl restart "$APP"
  fi
  if post_check; then
    say "Rolled back. If the newer version had changed the database schema, restore the pre-upgrade backup (see docs/backups.md)."
  else
    die "the previous version does not start either; see: journalctl -u $APP -n 100"
  fi
}

# ---------------------------------------------------------------- install

nologin_shell() {
  for s in /usr/sbin/nologin /sbin/nologin /bin/false; do
    [ -x "$s" ] && { echo "$s"; return; }
  done
  echo /bin/false
}

install_files() {
  step "Installing files" "re-run the script; it is safe to run more than once"
  if ! getent passwd "$APP" >/dev/null; then
    $SUDO useradd --system --user-group --no-create-home --home-dir "$STATE_DIR" \
      --shell "$(nologin_shell)" --comment "hwstats service" "$APP"
    info "created system user $APP"
  fi
  if have_systemd && systemctl is-active --quiet "$APP" 2>/dev/null; then
    info "stopping the running service for the upgrade"
    $SUDO systemctl stop "$APP"
  fi
  # `install` creates a new file, so SELinux gives it the right label.
  $SUDO install -D -m 0755 "$SRC_DIR/target/release/$APP" "$BIN"
  if command -v restorecon >/dev/null 2>&1; then $SUDO restorecon -F "$BIN" || true; fi
  info "installed $BIN"

  $SUDO install -d -m 0755 "$CONF_DIR"
  local def="$CONF_DIR/config.toml.default"
  if [ -f "$CONF" ]; then
    info "keeping existing $CONF"
    if [ -f "$def" ] && ! cmp -s "$SRC_DIR/packaging/config.toml" "$def"; then
      warn "the default configuration changed in this version; compare your $CONF with $def"
    fi
  else
    $SUDO install -m 0644 "$SRC_DIR/packaging/config.toml" "$CONF"
    info "installed $CONF"
  fi
  # Reference copy of the shipped defaults, used to detect changes on upgrades.
  $SUDO install -m 0644 "$SRC_DIR/packaging/config.toml" "$def"

  $SUDO install -d -m 0755 "$DOC_DIR"
  $SUDO install -m 0644 "$SRC_DIR/README.md" "$DOC_DIR/README.md"
  if [ -d "$SRC_DIR/docs" ]; then
    $SUDO install -d -m 0755 "$DOC_DIR/docs"
    $SUDO install -m 0644 "$SRC_DIR"/docs/*.md "$DOC_DIR/docs/"
  fi

  if have_systemd; then
    if [ -f "$UNIT" ] && ! cmp -s "$SRC_DIR/packaging/$APP.service" "$UNIT"; then
      info "updating the service unit (local changes belong in: sudo systemctl edit $APP)"
    fi
    $SUDO install -m 0644 "$SRC_DIR/packaging/$APP.service" "$UNIT"
    $SUDO systemctl daemon-reload
    $SUDO systemctl enable "$APP" >/dev/null 2>&1
    $SUDO systemctl restart "$APP"
    info "service enabled and started"
  fi
}

post_check() {
  have_systemd || { say "Done. Start it manually with: $BIN --config $CONF"; return 0; }
  step "Checking the service" "see the log with: journalctl -u $APP -n 100"
  local port ok=0
  port="$(config_port)"
  for _ in $(seq 1 30); do
    if curl -fsS --max-time 2 "http://127.0.0.1:$port/api/health" >/dev/null 2>&1; then ok=1; break; fi
    sleep 0.5
  done
  if [ "$ok" -ne 1 ]; then
    warn "the service did not answer on port $port within 15 s; recent log:"
    $SUDO journalctl -u "$APP" -n 50 --no-pager || true
    return 1
  fi
  sleep 3
  if ! systemctl is-active --quiet "$APP"; then
    warn "the service stopped right after starting; recent log:"
    $SUDO journalctl -u "$APP" -n 50 --no-pager || true
    return 1
  fi
  echo
  "$BIN" --config "$CONF" doctor || true
  echo
  say "hwstats is running: http://127.0.0.1:$port/"
}

# ---------------------------------------------------------------- uninstall

uninstall() {
  step "Removing the service"
  if have_systemd; then
    $SUDO systemctl disable --now "$APP" >/dev/null 2>&1 || true
    $SUDO rm -f "$UNIT"
    $SUDO systemctl daemon-reload || true
  fi
  $SUDO rm -f "$BIN" "$PREV"
  $SUDO rm -rf "$DOC_DIR"
  info "removed the binary, the unit and the documentation"
  if [ "$MODE" = purge ]; then
    if [ "$ASSUME_YES" -ne 1 ]; then
      printf 'This deletes all history, backups (%s) and the configuration (%s). Type "yes" to continue: ' "$STATE_DIR" "$CONF_DIR"
      local answer
      read -r answer
      [ "$answer" = "yes" ] || die "purge cancelled; the service is removed but data and config are kept"
    fi
    $SUDO rm -rf "$STATE_DIR" "$CONF_DIR"
    if getent passwd "$APP" >/dev/null; then $SUDO userdel "$APP" || true; fi
    info "removed data, backups, configuration and the $APP user"
  else
    info "kept $STATE_DIR (history and backups) and $CONF"
  fi
}

# ---------------------------------------------------------------- main

case "$MODE" in
  uninstall|purge)
    uninstall
    ;;
  build)
    detect_os
    preflight
    install_packages
    setup_rust
    build
    ;;
  rollback)
    rollback
    ;;
  install)
    detect_install
    detect_os
    preflight
    install_packages
    setup_rust
    build
    prepare_upgrade
    install_files
    if ! post_check; then
      if [ "$UPGRADE" -eq 1 ] && [ -x "$PREV" ]; then
        warn "the new version did not start; going back to the previous one"
        rollback
      fi
      exit 1
    fi
    ;;
esac
