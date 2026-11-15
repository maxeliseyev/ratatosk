#!/usr/bin/env bash
# Prepare this directory on a fresh VPS: .env, accounts.tsv, server.toml,
# firewall rules. Idempotent: an existing .env, accounts.tsv or server.toml
# is left alone, so re-running after `git pull` is safe.
#
#   bash setup.sh example.com          # first run on a new host
#   bash setup.sh                      # regenerate what is missing
#
# Then: docker compose up -d && ./accounts.sh add <label>
set -eu
cd "$(dirname "$0")"

DOMAIN=${1:-${DOMAIN:-}}
if [ -z "$DOMAIN" ]; then
    if [ -f .env ]; then
        DOMAIN=$(sed -n 's/^DOMAIN=//p' .env)
    fi
fi
if [ -z "$DOMAIN" ]; then
    echo "использование: bash setup.sh домен" >&2
    exit 1
fi

if [ -f .env ]; then
    echo ".env уже есть, не трогаю: $(grep '^DOMAIN=' .env || true)"
else
    cat >.env <<ENV
DOMAIN=$DOMAIN
RATATOSK_SERVER_IMAGE=ghcr.io/maxeliseyev/ratatosk-server:latest
ENV
    chmod 600 .env
    echo "создал .env для $DOMAIN"
fi

if [ ! -f accounts.tsv ]; then
    : >accounts.tsv
    chmod 600 accounts.tsv
    echo "создал пустой accounts.tsv"
fi

DOMAIN="$DOMAIN" bash ./accounts.sh regen

# Публичные порты стенда; 22 не трогаем — им занимается hardening.sh.
for rule in "80/tcp" "443/tcp" "443/udp"; do
    sudo ufw allow "$rule" comment 'citylivecam' >/dev/null || true
done

chmod 600 server.toml 2>/dev/null || true

echo
echo "дальше:"
echo "  docker compose up -d"
echo "  docker compose logs -f caddy        # ждать 'certificate obtained successfully'"
echo "  ./accounts.sh add phone             # выдаст ссылку h3://…"
