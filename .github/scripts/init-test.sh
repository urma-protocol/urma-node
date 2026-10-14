#!/bin/sh
# Runs inside a container whose init is OpenRC or runit: install, check, uninstall.
set -eu

kind=$1
adapter=$2

diagnose() {
    echo "--- diagnostics"
    urma-node service status || true
    case $kind in
    runit)
        sv status /etc/sv/urma-node || true
        ls -la /var/service/ /etc/sv/urma-node /etc/sv/urma-node/log || true
        tail -n 50 /var/log/urma-node/current || true
        ;;
    openrc)
        rc-service urma-node status || true
        tail -n 50 /var/log/urma-node/urma-node.log || true
        ;;
    esac
    ls -la /var/log/urma-node /var/lib/urma-node /etc/urma-node || true
    ps -ef || true
}
trap 'rc=$?; if [ "$rc" -ne 0 ]; then diagnose; fi; exit "$rc"' EXIT

install -m755 /bins/urma-node /bins/urma-node-control /usr/bin/

urma-node service install
test -e "$adapter"

listening=no
for _ in $(seq 60); do
    status=$(urma-node service status)
    if echo "$status" | grep -q "\"listening\": *true"; then
        listening=yes
        break
    fi
    sleep 2
done
echo "$status"
echo "$status" | grep -q "\"init\": *\"$kind\""
echo "$status" | grep -q "\"running\": *true"
test "$listening" = yes

urma-node-control --endpoint 127.0.0.1:28331 --token-file /var/lib/urma-node/service.token status >/dev/null

urma-node service uninstall
test ! -e "$adapter"
status=$(urma-node service status)
echo "$status"
echo "$status" | grep -q "\"registered\": *false"
echo "$kind lifecycle ok"
