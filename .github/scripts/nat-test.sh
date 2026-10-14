#!/usr/bin/env bash
# NAT traversal simulation in network namespaces. Root on Linux, needs
# iproute2, iptables and jq:
#   sudo nat-test.sh BIN_DIR FIXTURE_DIR cone|symmetric
# BIN_DIR holds urma-node and urma-node-control; FIXTURE_DIR is written by
#   URMA_FIXTURE_DIR=DIR cargo test -p urma-node --test nat_fixture -- --ignored
# and copied, never changed. Only B holds records.
#
#   P 11.0.1.10 (seed, relay) ── inet ─┬─ 11.0.2.10 nat-a ── 192.168.1.2 A
#                                      └─ 11.0.3.10 nat-b ── 192.168.2.2 B
#
# cone: MASQUERADE keeps the source port, hole punching opens a direct A-B
# link and A syncs B's inventory over it. symmetric: --random-fully maps
# every flow to a new port, the punch fails and nothing syncs over the relay.
set -euo pipefail

[ $# -eq 3 ] || { echo "usage: $0 BIN_DIR FIXTURE_DIR cone|symmetric" >&2; exit 2; }
bin=$(cd "$1" && pwd)
fixture=$(cd "$2" && pwd)
mode=$3
case $mode in
cone) masquerade=(-j MASQUERADE) ;;
symmetric) masquerade=(-j MASQUERADE --random-fully) ;;
*) echo "mode must be cone or symmetric" >&2; exit 2 ;;
esac

P_IP=11.0.1.10
NODES=(p a b)
NAMESPACES=(inet p nat-a nat-b a b)
work=$(mktemp -d)
declare -A pid id

at() { ip netns exec "urmanat-$1" "${@:2}"; }

control() {
    at "$1" "$bin/urma-node-control" --endpoint 127.0.0.1:28331 \
        --token-file "$work/$1/service.token" "${@:2}"
}

status() { control "$1" status; }

diagnose() {
    echo "--- diagnostics ($mode)"
    for node in "${NODES[@]}"; do
        echo "--- $node ${id[$node]:-} status (pid ${pid[$node]:-none})"
        kill -0 "${pid[$node]:-}" 2>/dev/null || echo "$node is not running"
        status "$node" || true
        echo "--- $node book"
        control "$node" overlay-book || true
        echo "--- $node log"
        tail -n 150 "$work/$node.log" || true
    done
    for router in nat-a nat-b; do
        echo "--- $router nat"
        at "$router" iptables -t nat -L -nv || true
        at "$router" iptables -L FORWARD -nv || true
    done
}

cleanup() {
    rc=$?
    [ "$rc" -eq 0 ] || diagnose
    for node in "${NODES[@]}"; do
        [ -n "${pid[$node]:-}" ] && kill -TERM "${pid[$node]}" 2>/dev/null || true
    done
    for _ in $(seq 20); do
        pgrep -f "^$bin/urma-node .*$work" >/dev/null || break
        sleep 0.5
    done
    for ns in "${NAMESPACES[@]}"; do
        ip netns pids "urmanat-$ns" 2>/dev/null | xargs -r kill -KILL || true
        ip netns del "urmanat-$ns" 2>/dev/null || true
    done
    rm -rf "$work"
    exit "$rc"
}
trap cleanup EXIT

# link NS_A IF_A ADDR_A NS_B IF_B ADDR_B
link() {
    ip link add "$2" netns "urmanat-$1" type veth peer name "$5" netns "urmanat-$4"
    at "$1" ip addr add "$3" dev "$2"
    at "$4" ip addr add "$6" dev "$5"
    at "$1" ip link set "$2" up
    at "$4" ip link set "$5" up
}

# A home router: LAN out through WAN, only replies come back in.
nat() {
    at "$1" sysctl -qw net.ipv4.ip_forward=1
    at "$1" iptables -t nat -A POSTROUTING -o wan "${masquerade[@]}"
    at "$1" iptables -P FORWARD DROP
    at "$1" iptables -A FORWARD -i lan -o wan -j ACCEPT
    at "$1" iptables -A FORWARD -i wan -o lan -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT
}

topology() {
    for ns in "${NAMESPACES[@]}"; do
        ip netns del "urmanat-$ns" 2>/dev/null || true
        ip netns add "urmanat-$ns"
        at "$ns" ip link set lo up
    done
    at inet sysctl -qw net.ipv4.ip_forward=1
    link inet to-p 11.0.1.1/24 p eth0 "$P_IP/24"
    link inet to-a 11.0.2.1/24 nat-a wan 11.0.2.10/24
    link inet to-b 11.0.3.1/24 nat-b wan 11.0.3.10/24
    link nat-a lan 192.168.1.1/24 a eth0 192.168.1.2/24
    link nat-b lan 192.168.2.1/24 b eth0 192.168.2.2/24
    at p ip route add default via 11.0.1.1
    at nat-a ip route add default via 11.0.2.1
    at nat-b ip route add default via 11.0.3.1
    at a ip route add default via 192.168.1.1
    at b ip route add default via 192.168.2.1
    nat nat-a
    nat nat-b
    at a ping -c 1 -W 2 "$P_IP" >/dev/null
    at b ping -c 1 -W 2 "$P_IP" >/dev/null
    if at inet ping -c 1 -W 1 192.168.1.2 >/dev/null 2>&1; then
        echo "A is reachable from outside its NAT" >&2
        exit 1
    fi
}

# Chain sync is pinned to a closed local port: the fixture's headers are
# already selected and nothing leaves the topology.
serve() {
    local node=$1
    at "$node" "$bin/urma-node" --data "$work/$node/data" --config "$work/$node/config.json" \
        serve --listen 127.0.0.1:28331 --token-file "$work/$node/service.token" \
        --p2p --no-commit-public --chain-peer 127.0.0.1:9 \
        --peer-key "$work/$node/peer.key" --urma-listen 0.0.0.0:28332 --no-seeds "${@:2}" \
        >"$work/$node.log" 2>&1 &
    pid[$node]=$!
    wait_for 30 "$node overlay up" up "$node"
    id[$node]=$(status "$node" | jq -r .overlay.peer_id)
}

up() { status "$1" | jq -e .overlay.peer_id >/dev/null; }

# wait_for SECONDS WHAT COMMAND...
wait_for() {
    local deadline=$((SECONDS + $1)) what=$2
    shift 2
    until "$@" >/dev/null 2>&1; do
        if [ "$SECONDS" -ge "$deadline" ]; then
            echo "timed out: $what" >&2
            return 1
        fi
        sleep 2
    done
    echo "ok: $what"
}

# The overlay.peers entry of PEER on NODE, or null.
peer() {
    status "$1" | jq -c --arg id "${id[$2]}" '[(.overlay.peers // [])[] | select(.label == $id)][0]'
}

listed() { peer "$1" "$2" | jq -e . >/dev/null; }

linked() { [ "$(peer "$1" "$2" | jq -r .link)" = "$3" ]; }

# A book entry for B that holds a circuit address through P.
circuit() {
    control "$1" overlay-book | jq -e --arg target "${id[$2]}" --arg relay "/p2p/${id[$3]}/p2p-circuit" \
        'any(.[]; (.peer_id == $target or any(.addrs[]; contains($target)))
            and any(.addrs[]; contains($relay)))' >/dev/null
}

reachability() { [ "$(status "$1" | jq -r .overlay.reachability)" = "$2" ]; }

# NODE holds a relay reservation on P.
reserved() {
    status "$1" | jq -e --arg relay "${id[p]}" 'any(.overlay.relays[]?; . == $relay)' >/dev/null
}

punch_failed() {
    status "$1" | jq -e '.overlay.hole_punch.last_error != null' >/dev/null
}

punched() {
    status "$1" | jq -e '.overlay.hole_punch.successes >= 1' >/dev/null
}

# A holds every transaction B scanned and B's cursor covers B's tip.
synced() {
    local tip held want
    tip=$(status b | jq .archive.selected_tip.height)
    want=$(status b | jq .archive.stored_transactions)
    held=$(status a | jq .archive.stored_transactions)
    [ "$held" = "$want" ] && [ "$(peer a b | jq .synced_through)" = "$tip" ]
}

nothing_synced() { [ "$(peer a b | jq .synced_through)" = null ]; }

either() { "$1" a || "$1" b; }

topology
for node in "${NODES[@]}"; do
    mkdir -m 700 "$work/$node"
    cp -R --preserve=mode "$fixture/$node/." "$work/$node/"
done
serve p --urma-announce "/ip4/$P_IP/tcp/28332" --urma-announce "/ip4/$P_IP/udp/28332/quic-v1"
seed=/ip4/$P_IP/tcp/28332/p2p/${id[p]}
serve a --urma-seed "$seed"
serve b --urma-seed "$seed"

wait_for 120 "A reaches P" listed a p
wait_for 120 "B reaches P" listed b p
wait_for 60 "P lists A and B" eval 'listed p a && listed p b'
wait_for 120 "P public, A and B private" \
    eval 'reachability p public && reachability a private && reachability b private'
wait_for 120 "A and B reserve on P" eval 'reserved a && reserved b'
wait_for 180 "A learns B's circuit address through P" circuit a b p
wait_for 60 "A-P and B-P links are direct" eval 'linked a p direct && linked b p direct'

case $mode in
cone)
    wait_for 180 "direct A-B link" eval 'linked a b direct && linked b a direct'
    wait_for 60 "hole punch success recorded" either punched
    wait_for 120 "A syncs B's inventory over the direct link" synced
    ;;
symmetric)
    wait_for 180 "hole punch failure recorded" either punch_failed
    sleep 30
    if linked a b direct || linked b a direct; then
        echo "direct A-B link through symmetric NAT" >&2
        exit 1
    fi
    nothing_synced || { echo "A synced from B without a direct link" >&2; exit 1; }
    echo "ok: no direct link, nothing synced over the relay"
    ;;
esac
echo "nat $mode ok"
