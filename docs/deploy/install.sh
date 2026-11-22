#!/usr/bin/env bash
# HostingOS — install (or manually activate) a signed release as a bare
# binary under systemd. See docs/UPDATES.md and docs/DEPLOYMENT.md.
#
# Do not pipe this script into a shell from the network. Download it, read
# it, then run it:
#
#   sudo ./install.sh --version 0.2.0 --repo owner/name --pubkey hostingos.pub
#   sudo ./install.sh --version 0.2.0 --archive ./hostingos-0.2.0-x86_64-unknown-linux-musl.tar.gz \
#        --pubkey hostingos.pub        # offline: the .minisig must sit next to the archive
#
# What it does, in order:
#   1. checks the arguments, the platform and the tools it needs (minisign is
#      mandatory: nothing is installed without a valid signature);
#   2. downloads the release archive and its signature over HTTPS only (or
#      takes local files), verifies the minisign signature against the public
#      key you pass, and checks the archive listing (one top directory, no
#      absolute paths, no `..`, no links or devices) before unpacking;
#   3. creates the system user `hostingos` and the layout below --prefix
#      (default /opt/hostingos): releases/<version>/, current, state/;
#   4. on a fresh install: points `current` at the release, installs the
#      systemd unit (unless one exists) and an environment file template
#      (never overwrites an existing one). With an existing install it only
#      unpacks the release; pass --activate to switch `current` to it (the
#      admin panel's update page, /admin/server/updates, is the safer way:
#      backup, migrations, health check and automatic rollback).
#
# It never starts the service and never runs migrations on its own; it
# prints the next steps instead.

set -euo pipefail
umask 022

PREFIX=/opt/hostingos
VERSION=""
REPO=""
ARCHIVE=""
PUBKEY=""
ACTIVATE=0
SERVICE_USER=hostingos
ENV_DIR=/etc/hostingos
UNIT_DIR=/etc/systemd/system

die() { printf 'install.sh: %s\n' "$*" >&2; exit 1; }
note() { printf '==> %s\n' "$*"; }

usage() {
    sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'
    exit "${1:-0}"
}

while [ $# -gt 0 ]; do
    case "$1" in
        --version) VERSION="${2:-}"; shift 2 ;;
        --repo) REPO="${2:-}"; shift 2 ;;
        --archive) ARCHIVE="${2:-}"; shift 2 ;;
        --pubkey) PUBKEY="${2:-}"; shift 2 ;;
        --prefix) PREFIX="${2:-}"; shift 2 ;;
        --activate) ACTIVATE=1; shift ;;
        -h|--help) usage 0 ;;
        *) die "unknown argument: $1 (see --help)" ;;
    esac
done

# --- arguments ---------------------------------------------------------------
[ "$(id -u)" -eq 0 ] || die "run as root (it creates a system user and $PREFIX)"
[ "$(uname -s)" = Linux ] || die "Linux only"
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] \
    || die "--version must be a release version like 0.2.0 (got: '${VERSION}')"
[ -n "$PUBKEY" ] && [ -f "$PUBKEY" ] || die "--pubkey FILE (the project's minisign public key) is required"
case "$PREFIX" in
    /*) ;;
    *) die "--prefix must be an absolute path" ;;
esac
[[ "$PREFIX" != *[[:space:]]* ]] || die "--prefix must not contain spaces"
if [ -z "$ARCHIVE" ]; then
    [[ "$REPO" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] \
        || die "--repo owner/name (GitHub) or --archive FILE is required"
fi

case "$(uname -m)" in
    x86_64|amd64) ARCH=x86_64 ;;
    aarch64|arm64) ARCH=aarch64 ;;
    *) die "unsupported architecture $(uname -m) (x86_64 and aarch64 only)" ;;
esac
TARGET="${ARCH}-unknown-linux-musl"
NAME="hostingos-${VERSION}-${TARGET}"

for tool in minisign tar gzip install ln mv runuser useradd; do
    command -v "$tool" >/dev/null 2>&1 || die "missing tool: $tool (minisign: https://jedisct1.github.io/minisign/)"
done
if [ -z "$ARCHIVE" ]; then
    command -v curl >/dev/null 2>&1 || die "missing tool: curl"
fi

WORK="$(mktemp -d)"
cleanup() { rm -rf -- "$WORK"; }
trap cleanup EXIT

# --- fetch -------------------------------------------------------------------
if [ -n "$ARCHIVE" ]; then
    [ -f "$ARCHIVE" ] || die "archive not found: $ARCHIVE"
    [ -f "${ARCHIVE}.minisig" ] || die "signature not found: ${ARCHIVE}.minisig"
    cp -- "$ARCHIVE" "$WORK/${NAME}.tar.gz"
    cp -- "${ARCHIVE}.minisig" "$WORK/${NAME}.tar.gz.minisig"
else
    BASE="https://github.com/${REPO}/releases/download/v${VERSION}"
    for f in "${NAME}.tar.gz" "${NAME}.tar.gz.minisig"; do
        note "downloading $f"
        # HTTPS only (also for redirects), TLS 1.2+, fail on HTTP errors,
        # bounded size and time.
        curl --proto '=https' --proto-redir '=https' --tlsv1.2 -fsSL \
            --max-filesize 536870912 --max-time 900 --retry 3 \
            -o "$WORK/$f" "$BASE/$f"
    done
fi

# --- verify ------------------------------------------------------------------
note "verifying the signature"
minisign -V -q -p "$PUBKEY" -m "$WORK/${NAME}.tar.gz" -x "$WORK/${NAME}.tar.gz.minisig" \
    || die "signature verification FAILED — do not install this archive"

note "checking the archive listing"
tar -tzf "$WORK/${NAME}.tar.gz" > "$WORK/listing"
while IFS= read -r entry; do
    case "$entry" in
        "${NAME}"|"${NAME}/"|"${NAME}/"*) ;;
        *) die "unexpected path in archive: $entry" ;;
    esac
    case "/$entry/" in
        */../*|*/./*) die "path traversal in archive: $entry" ;;
    esac
done < "$WORK/listing"
# Only regular files and directories (first column of the verbose listing).
# Written to a file first: with pipefail, an early-exiting grep could make
# the pipeline fail and the check pass silently.
tar -tvzf "$WORK/${NAME}.tar.gz" > "$WORK/types"
if cut -c1 "$WORK/types" | grep -v '^[-d]$' > "$WORK/bad"; [ -s "$WORK/bad" ]; then
    die "the archive contains links or special files"
fi

# --- user and layout ------------------------------------------------------------
if ! id "$SERVICE_USER" >/dev/null 2>&1; then
    note "creating system user $SERVICE_USER"
    useradd --system --home-dir "$PREFIX" --no-create-home --shell /usr/sbin/nologin "$SERVICE_USER"
fi
install -d -m 0755 -o "$SERVICE_USER" -g "$SERVICE_USER" "$PREFIX" "$PREFIX/releases"
install -d -m 0700 -o "$SERVICE_USER" -g "$SERVICE_USER" "$PREFIX/state"

DEST="$PREFIX/releases/$VERSION"
[ ! -e "$DEST" ] || die "$DEST already exists (remove it first to reinstall this version)"

STAGING="$(mktemp -d "$PREFIX/releases/.staging-${VERSION}-XXXXXX")"
tar -xzf "$WORK/${NAME}.tar.gz" -C "$STAGING" --no-same-owner --no-same-permissions
[ -x "$STAGING/$NAME/hostingos" ] && [ -d "$STAGING/$NAME/static" ] && [ -d "$STAGING/$NAME/locales" ] \
    || { rm -rf -- "$STAGING"; die "the archive is not a HostingOS release"; }
chown -R "$SERVICE_USER:$SERVICE_USER" "$STAGING"
# Smoke test as the service user (never as root), from a neutral directory.
REPORT="$(cd / && runuser -u "$SERVICE_USER" -- "$STAGING/$NAME/hostingos" version --json)" \
    || { rm -rf -- "$STAGING"; die "the new binary does not run on this host"; }
printf '%s' "$REPORT" | grep -qF "\"version\":\"${VERSION}\"" \
    || { rm -rf -- "$STAGING"; die "the binary does not report version $VERSION"; }
mv -T -- "$STAGING/$NAME" "$DEST"
rmdir -- "$STAGING"
note "release unpacked to $DEST"

# Atomic symlink switch: create next to the target, then rename over it.
switch_link() { # name target
    ln -sfn -- "$2" "$PREFIX/.$1.tmp"
    mv -T -- "$PREFIX/.$1.tmp" "$PREFIX/$1"
    chown -h "$SERVICE_USER:$SERVICE_USER" "$PREFIX/$1"
}

FRESH=0
if [ ! -L "$PREFIX/current" ]; then
    switch_link current "releases/$VERSION"
    FRESH=1
    note "current -> releases/$VERSION"
elif [ "$ACTIVATE" -eq 1 ]; then
    OLD="$(readlink "$PREFIX/current")"
    switch_link previous "$OLD"
    switch_link current "releases/$VERSION"
    note "previous -> $OLD, current -> releases/$VERSION"
fi

# --- configuration and unit (never overwritten) ------------------------------
install -d -m 0750 -o root -g "$SERVICE_USER" "$ENV_DIR"
if [ ! -e "$ENV_DIR/hostingos.env" ]; then
    install -m 0640 -o root -g "$SERVICE_USER" /dev/null "$ENV_DIR/hostingos.env"
    cat > "$ENV_DIR/hostingos.env" <<'ENV'
# HostingOS configuration (docs/DEPLOYMENT.md). Fill in, then start the service.
DATABASE_URL=
DATABASE_URL_DIRECT=
# openssl rand -base64 32 — back it up; losing it makes stored credentials unreadable.
HOS_SECRET_KEY=
HOS_BIND=127.0.0.1:8080
# The hosting company's public URL (storefront /, client area /client, admin /admin).
HOS_BASE_URL=https://panel.example.com
HOS_TRUST_PROXY=true
# `private`: integrations may reach your LAN. `public`: public addresses only.
HOS_OUTBOUND_POLICY=private
# Updates (docs/UPDATES.md): checks stay off until a source is set.
HOS_UPDATE_REPO=
# Optional: command that backs up the database before an update.
# HOS_UPDATE_BACKUP_COMMAND=
ENV
    note "wrote $ENV_DIR/hostingos.env (edit it)"
fi
UNIT_SRC="$DEST/deploy/hostingos.service"
if [ ! -e "$UNIT_DIR/hostingos.service" ] && [ -f "$UNIT_SRC" ]; then
    if [ "$PREFIX" = /opt/hostingos ]; then
        install -m 0644 "$UNIT_SRC" "$UNIT_DIR/hostingos.service"
        systemctl daemon-reload || true
        note "installed $UNIT_DIR/hostingos.service"
    else
        note "custom --prefix: adapt $UNIT_SRC (paths) and copy it to $UNIT_DIR yourself"
    fi
fi

# --- next steps ----------------------------------------------------------------
if [ "$FRESH" -eq 1 ]; then
    cat <<EOF

Next steps:
  1. edit $ENV_DIR/hostingos.env
  2. apply the migrations and create the company and its owner (prints the password):
       sudo -u $SERVICE_USER sh -c 'set -a; . $ENV_DIR/hostingos.env; exec $PREFIX/current/hostingos setup --email you@example.com --company "Your Hosting Company"'
  3. sudo systemctl enable --now hostingos
  4. sign in at HOS_BASE_URL/auth/login
EOF
elif [ "$ACTIVATE" -eq 1 ]; then
    cat <<EOF

Activated $VERSION. Run its migrations, then restart:
  sudo -u $SERVICE_USER sh -c 'set -a; . $ENV_DIR/hostingos.env; exec $PREFIX/current/hostingos migrate'
  sudo systemctl restart hostingos
Back: point current at the previous release (hostingos update rollback) and restore
the database backup if the release was not rollback-safe (docs/UPDATES.md).
EOF
else
    cat <<EOF

$VERSION is unpacked but not active. Install it from the admin panel
(Server -> Updates, /admin/server/updates: backup, migrations, health check,
automatic rollback), or re-run with --activate.
EOF
fi
