#!/bin/sh
# The container has no network manager; this stands in for the one that provides `net` on a real install.
set -eu
printf '%s\n' '#!/sbin/openrc-run' 'description="CI stand-in for a network manager"' \
    'depend() { provide net; }' 'start() { return 0; }' >/etc/init.d/ci-net
chmod 755 /etc/init.d/ci-net
rc-update add ci-net default
rc-service ci-net start
