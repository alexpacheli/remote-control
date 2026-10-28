#!/usr/bin/env bash
# Installs (or updates) the Hext Remote services on the VM: hext-server and
# iroh-relay behind Caddy. Run from the repository root, as a user with sudo:
#
#   ./deploy/install.sh
#
# Safe to run again (to update after a `git pull`): it rebuilds, keeps the
# database and the secret, and restarts the services.
set -euo pipefail

cd "$(dirname "$0")/.."
IROH_RELAY_VERSION=1.4.1

step() { printf '\n==> %s\n' "$*"; }

step "Caddy"
if ! command -v caddy >/dev/null; then
    sudo apt-get install -y debian-keyring debian-archive-keyring apt-transport-https curl gnupg
    curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/gpg.key' |
        sudo gpg --dearmor --yes -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
    curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt' |
        sudo tee /etc/apt/sources.list.d/caddy-stable.list >/dev/null
    sudo apt-get update
    sudo apt-get install -y caddy
fi

step "Building hext-server (a few minutes)"
if ! command -v cargo >/dev/null; then
    # shellcheck disable=SC1091
    source "$HOME/.cargo/env"
fi
cargo build --release --locked -p hext-server
sudo install -m 755 target/release/hext-server /usr/local/bin/hext-server

step "iroh-relay $IROH_RELAY_VERSION"
if ! /usr/local/bin/iroh-relay --version 2>/dev/null | grep -q "$IROH_RELAY_VERSION"; then
    cargo install iroh-relay --version "$IROH_RELAY_VERSION" --features server --locked \
        --root "$HOME/.local/iroh-relay"
    sudo install -m 755 "$HOME/.local/iroh-relay/bin/iroh-relay" /usr/local/bin/iroh-relay
fi

step "Configuration"
sudo mkdir -p /etc/hext
if ! sudo test -f /etc/hext/secrets.env; then
    # The secret the relay presents to hext-server when asking about a device.
    token=$(head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')
    printf 'HEXT_RELAY_TOKEN=%s\nIROH_RELAY_HTTP_BEARER_TOKEN=%s\n' "$token" "$token" |
        sudo tee /etc/hext/secrets.env >/dev/null
    sudo chmod 600 /etc/hext/secrets.env
fi
sudo install -m 644 deploy/iroh-relay.toml /etc/hext/iroh-relay.toml
sudo install -m 644 deploy/Caddyfile /etc/caddy/Caddyfile
sudo install -m 644 deploy/hext-server.service /etc/systemd/system/hext-server.service
sudo install -m 644 deploy/iroh-relay.service /etc/systemd/system/iroh-relay.service
sudo systemctl daemon-reload

step "Firewall (80, 443 TCP; 7842 UDP; IPv4 and IPv6)"
# Oracle's images reject what is not listed: allow before the REJECT rule.
allow() {
    for table in iptables ip6tables; do
        command -v "$table" >/dev/null || continue
        if ! sudo "$table" -C INPUT -p "$1" --dport "$2" -j ACCEPT 2>/dev/null; then
            reject=$(sudo "$table" -L INPUT --line-numbers -n | awk '/REJECT/ {print $1; exit}')
            sudo "$table" -I INPUT "${reject:-1}" -p "$1" --dport "$2" -j ACCEPT
        fi
    done
}
allow tcp 80
allow tcp 443
allow udp 7842
# The port of the manual test, if it is still open.
sudo iptables -D INPUT -p tcp --dport 8080 -j ACCEPT 2>/dev/null || true
if command -v netfilter-persistent >/dev/null; then
    sudo netfilter-persistent save
fi

step "Services"
# The old relay (on 80/443 by itself) gives way to Caddy.
sudo systemctl stop iroh-relay 2>/dev/null || true
sudo systemctl enable --now caddy
sudo systemctl reload caddy
sudo systemctl enable --now hext-server
sudo systemctl restart hext-server
sudo systemctl enable --now iroh-relay
sudo systemctl restart iroh-relay

step "Checks"
sleep 5
check() {
    if curl -fsS --max-time 10 "$1" >/dev/null; then
        echo "ok    $1"
    else
        echo "FALHA $1"
    fi
}
check https://signal.hext.com.br/health
check https://relay.hext.com.br/generate_204
systemctl --no-pager --lines=0 status caddy hext-server iroh-relay | grep -E '●|Active'
echo
echo "Logs: journalctl -u hext-server -u iroh-relay -u caddy -f"
