#!/usr/bin/env bash
# Install, upgrade, remove and purge the deb under the runner's systemd.
set -euo pipefail

first=$(realpath "$1")
second=$(realpath "$2")
unit=/etc/systemd/system/urma-node.service
token=/var/lib/urma-node/service.token

absent() {
    if "$@" >/dev/null 2>&1; then
        echo "unexpected success: $*" >&2
        exit 1
    fi
}

control() {
    sg urma-node -c "urma-node-control --endpoint 127.0.0.1:28331 --token-file $token status"
}

wait_listening() {
    for _ in $(seq 60); do
        status=$(sudo urma-node service status)
        if echo "$status" | grep -q '"listening": *true'; then
            echo "$status"
            echo "$status" | grep -q '"init": *"systemd"'
            echo "$status" | grep -q '"running": *true'
            control
            return 0
        fi
        sleep 2
    done
    echo "$status"
    ls -la /var/lib/urma-node
    id
    sudo journalctl -u urma-node --no-pager | tail -50
    return 1
}

sudo apt-get install -y "$first"
systemctl is-active urma-node
systemctl is-enabled urma-node
test -f "$unit"
id -nG "$USER" | tr ' ' '\n' | grep -qx urma-node
wait_listening
before=$(systemctl show -p MainPID --value urma-node)

sudo apt-get install -y "$second"
dpkg-query -W -f '${Version}\n' urma-node
systemctl is-active urma-node
after=$(systemctl show -p MainPID --value urma-node)
echo "MainPID $before -> $after"
test "$after" != 0
test "$before" != "$after"
wait_listening

sudo apt-get remove -y urma-node
absent systemctl is-active urma-node
test ! -e "$unit"
absent systemctl cat urma-node
sudo test -d /var/lib/urma-node/data
getent passwd urma-node

sudo apt-get purge -y urma-node
test ! -e /var/lib/urma-node
test ! -e /etc/urma-node
test ! -e /var/log/urma-node
absent getent passwd urma-node
echo "deb lifecycle ok"
