#!/bin/sh
# Runs inside a container whose init is OpenRC or runit: install, check, uninstall.
set -eu

kind=$1
adapter=$2
install -m755 /bins/urma-node /bins/urma-node-control /usr/bin/

urma-node service install
test -e "$adapter"

running=no
for _ in $(seq 60); do
    status=$(urma-node service status)
    if echo "$status" | grep -q "\"running\": *true"; then
        running=yes
        break
    fi
    sleep 2
done
echo "$status"
echo "$status" | grep -q "$kind"
test "$running" = yes

for _ in $(seq 60); do
    if urma-node-control --endpoint 127.0.0.1:28331 --token-file /var/lib/urma-node/service.token status; then
        break
    fi
    sleep 2
done
urma-node-control --endpoint 127.0.0.1:28331 --token-file /var/lib/urma-node/service.token status >/dev/null

urma-node service uninstall
test ! -e "$adapter"
status=$(urma-node service status)
echo "$status"
echo "$status" | grep -q "\"registered\": *false"
echo "$kind lifecycle ok"
