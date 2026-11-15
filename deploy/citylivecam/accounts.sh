#!/usr/bin/env bash
# Manage the h3 server's accounts. accounts.tsv (label<TAB>uuid) is the
# source of truth; server.toml is regenerated from it and applied with
# `docker compose kill -s HUP ratatosk-server` (no dropped connections).
#
#   ./accounts.sh add phone | list | rm phone | regen | apply
#
# secrets (accounts.tsv, server.toml, .env) stay on the server and are
# git-ignored: a new host only needs this directory from the repository.
set -euo pipefail

DIR="$(cd "$(dirname "$0")" && pwd)"
TSV="$DIR/accounts.tsv"
CONF="$DIR/server.toml"

# .env is the same file docker compose reads.
if [ -f "$DIR/.env" ]; then
    set -a
    # shellcheck disable=SC1091
    . "$DIR/.env"
    set +a
fi
DOMAIN="${DOMAIN:?set DOMAIN in .env}"
SITE_BACKEND="${SITE_BACKEND:-http://citycam:8080}"
CERT_DIR="/data/caddy/certificates/acme-v02.api.letsencrypt.org-directory/$DOMAIN"

[ -f "$TSV" ] || : >"$TSV"

usage() { echo "usage: $0 add <label> | list | rm <label> | regen | apply" >&2; exit 1; }

link() { printf 'h3://%s@%s:443?sni=%s#%s\n' "$2" "$DOMAIN" "$DOMAIN" "$1"; }

regen() {
    {
        printf 'listen = "0.0.0.0:443"\n'
        printf 'cert = "%s/%s.crt"\n' "$CERT_DIR" "$DOMAIN"
        printf 'key = "%s/%s.key"\n' "$CERT_DIR" "$DOMAIN"
        printf 'users = [\n'
        while IFS=$'\t' read -r _ uuid; do
            [ -n "${uuid:-}" ] || continue
            printf '  "%s",\n' "$uuid"
        done <"$TSV"
        printf ']\n'
        printf 'site_backend = "%s"\n' "$SITE_BACKEND"
        # Uncomment to change the flood limits (defaults: 1024/1024/16).
        # printf 'max_connections = 1024\n'
        # printf 'max_incoming = 1024\n'
        # printf 'max_connections_per_ip = 16\n'
    } >"$CONF"
    echo "regenerated $CONF"
}

apply() {
    (cd "$DIR" && docker compose kill -s HUP ratatosk-server)
    echo "signalled ratatosk-server to reload"
}

cmd="${1:-}"; shift || true
case "$cmd" in
    add)
        [ $# -eq 1 ] || usage
        label="$1"
        if grep -q "^${label}"$'\t' "$TSV"; then echo "label $label exists" >&2; exit 1; fi
        uuid="$(cat /proc/sys/kernel/random/uuid)"
        printf '%s\t%s\n' "$label" "$uuid" >>"$TSV"
        chmod 600 "$TSV"
        regen
        link "$label" "$uuid"
        ;;
    list)
        while IFS=$'\t' read -r label uuid; do
            [ -n "${uuid:-}" ] || continue
            printf '%-10s %s\n' "$label" "$(link "$label" "$uuid")"
        done <"$TSV"
        ;;
    rm)
        [ $# -eq 1 ] || usage
        label="$1"
        grep -v "^${label}"$'\t' "$TSV" >"$TSV.tmp" && mv "$TSV.tmp" "$TSV"
        regen
        ;;
    regen) regen ;;
    apply) apply ;;
    *) usage ;;
esac
