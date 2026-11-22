#!/usr/bin/env bash
# Signs every release file in a directory with minisign and checks each
# signature against the keys the shipped binary trusts.
#
#   MINISIGN_SECRET_KEY=<contents of the .key file> MINISIGN_PASSWORD=<…> \
#     .github/scripts/sign-release.sh dist "hostingos 0.2.0"
#
# * The secret key only ever exists in a 0600 file under $RUNNER_TEMP and is
#   removed on exit; the password goes to minisign on stdin.
# * Signatures must be pre-hashed (algorithm "ED"): the updater refuses
#   legacy ones.
# * Each signature must verify with a key of crates/hos-update/keys/
#   trusted.pub (or next.pub during a rotation): a release signed with a key
#   the binaries do not trust would be useless, so it fails here instead.
set -euo pipefail
umask 077

dir="${1:?usage: sign-release.sh DIR COMMENT}"
comment="${2:?usage: sign-release.sh DIR COMMENT}"
[ -n "${MINISIGN_SECRET_KEY:-}" ] || {
    echo "::error::secret MINISIGN_SECRET_KEY is not set (docs/RELEASING.md)" >&2
    exit 1
}

tmp="$(mktemp -d "${RUNNER_TEMP:-/tmp}/sign.XXXXXX")"
trap 'rm -rf -- "$tmp"' EXIT
printf '%s\n' "$MINISIGN_SECRET_KEY" > "$tmp/release.key"

# Public keys the binaries trust, one minisign.pub-style file each.
n=0
for f in crates/hos-update/keys/trusted.pub crates/hos-update/keys/next.pub; do
    [ -f "$f" ] || continue
    while read -r key _; do
        case "$key" in ''|'#'*) continue ;; esac
        n=$((n + 1))
        printf 'untrusted comment: release key %s\n%s\n' "$n" "$key" > "$tmp/pub.$n"
    done < "$f"
done
if [ "$n" -eq 0 ]; then
    echo "::error::crates/hos-update/keys/trusted.pub contains no public key; add the release key first (docs/RELEASING.md)" >&2
    exit 1
fi

shopt -s nullglob
for file in "$dir"/*; do
    case "$file" in *.minisig) continue ;; esac
    [ -f "$file" ] || continue
    printf '%s\n' "${MINISIGN_PASSWORD:-}" \
        | minisign -S -s "$tmp/release.key" -m "$file" -t "$comment $(basename "$file")" >/dev/null
    # Second line of the signature file: base64 of "ED" + key id + signature.
    sed -n 2p "$file.minisig" | base64 -d > "$tmp/sig.bin"
    alg="$(head -c 2 "$tmp/sig.bin")"
    if [ "$alg" != "ED" ]; then
        echo "::error::$file: not a pre-hashed (ED) signature; upgrade minisign" >&2
        exit 1
    fi
    ok=0
    for pub in "$tmp"/pub.*; do
        if minisign -V -q -p "$pub" -m "$file" >/dev/null 2>&1; then
            ok=1
            break
        fi
    done
    if [ "$ok" -ne 1 ]; then
        echo "::error::$file: the signing key is not in trusted.pub / next.pub" >&2
        exit 1
    fi
    echo "signed $(basename "$file")"
done
