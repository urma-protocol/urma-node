#!/usr/bin/env bash
# NAT traversal simulation in network namespaces. Root on Linux, needs
# iproute2, iptables and jq:
#   sudo nat-test.sh BIN_DIR FIXTURE_DIR cone|symmetric|cone-proxy|cone-restart|cone-kill
# BIN_DIR holds urma-node and urma-node-control; FIXTURE_DIR is written by
#   URMA_FIXTURE_DIR=DIR cargo test -p urma-node --test nat_fixture -- --ignored
# and copied, never changed: A scanned all four blocks, B the first two, P
# none. Optional environment
# (pass through sudo with `sudo env NAT_LOG=... nat-test.sh ...`):
#   NAT_LOG   RUST_LOG filter for the three daemons
#   NAT_KEEP  directory that receives each daemon's log and status timeline
#
#   P 11.0.1.10 (seed, relay) ── inet ─┬─ 11.0.2.10 nat-a ── 192.168.1.2 A
#                                      └─ 11.0.3.10 nat-b ── 192.168.2.2 B
#
# The WAN side is 11.0.0.0/8 on purpose: AutoNAT and the public-address
# filters treat it as public. Reserved ranges such as 198.18.0.0/15 are
# never advertised, so circuit addresses through them are invalid.
#
# cone: MASQUERADE keeps the source port, hole punching opens a direct A-B
# link, B syncs A's records and A B's coverage over it. symmetric:
# --random-fully maps every flow to a new port, the punch fails and nothing
# syncs over the relay.
# cone-proxy: cone, with P behind a TCP proxy like Fly's: every inbound TCP
# peer reaches P from one private address (172.16.0.1) while UDP keeps its
# real source, and A and B are seeded TCP first, then QUIC, as the dnsaddr
# TXT lists them. Expected to pass like cone.
# cone-restart: cone, then A stops on SIGINT and restarts with the same key
# and archive; within 60 s it reserves on P again and a new punch links it
# directly to B.
# cone-kill: cone-restart with SIGKILL, so A closes nothing on the way out.
set -euo pipefail

usage="usage: $0 BIN_DIR FIXTURE_DIR cone|symmetric|cone-proxy|cone-restart|cone-kill"
[ $# -eq 3 ] || { echo "$usage" >&2; exit 2; }
bin=$(cd "$1" && pwd)
fixture=$(cd "$2" && pwd)
mode=$3
case $mode in
cone | cone-proxy | cone-restart | cone-kill) masquerade=(-j MASQUERADE) ;;
symmetric) masquerade=(-j MASQUERADE --random-fully) ;;
*) echo "$usage" >&2; exit 2 ;;
esac

# The namespace names are fixed and a run clears leftovers: one run at a time.
exec 9>/run/urmanat.lock
flock -n 9 || { echo "waiting for another nat-test run" >&2; flock 9; }

stop_signal=INT
[ "$mode" = cone-kill ] && stop_signal=KILL

P_IP=11.0.1.10
NODES=(p a b)
NAMESPACES=(inet p nat-a nat-b a b)
work=$(mktemp -d)
declare -A pid=() id=() seen=() again=() base=()
exec 7>&1 8>&2

at() { ip netns exec "urmanat-$1" "${@:2}"; }

control() {
    at "$1" "$bin/urma-node-control" --endpoint 127.0.0.1:28331 \
        --token-file "$work/$1/service.token" "${@:2}"
}

status() { control "$1" status; }

diagnose() {
    echo "--- diagnostics ($mode), events seen: ${!seen[*]}, after restart: ${!again[*]}"
    for node in "${NODES[@]}"; do
        echo "--- $node ${id[$node]:-} status (pid ${pid[$node]:-none})"
        kill -0 "${pid[$node]:-}" 2>/dev/null || echo "$node is not running"
        status "$node" || true
        echo "--- $node book"
        control "$node" overlay-book || true
        echo "--- $node log"
        tail -n 150 "$work/$node.log" || true
        echo "--- $node timeline (changes only)"
        awk '{k = $0; sub(/^\{"t":[0-9]+,/, "{", k); if (k != last) print; last = k}' \
            "$work/$node.timeline" 2>/dev/null || true
    done
    for router in nat-a nat-b; do
        echo "--- $router nat"
        at "$router" iptables -t nat -L -nv || true
        at "$router" iptables -L FORWARD -nv || true
    done
}

cleanup() {
    rc=$?
    exec 1>&7 2>&8
    [ -n "${sampler:-}" ] && kill "$sampler" 2>/dev/null || true
    [ "$rc" -eq 0 ] || diagnose
    for node in "${NODES[@]}"; do
        [ -n "${pid[$node]:-}" ] && kill -TERM "${pid[$node]}" 2>/dev/null || true
    done
    for _ in $(seq 20); do
        pgrep -f "^$bin/urma-node .*$work" >/dev/null || break
        sleep 0.5
    done
    teardown
    if [ -n "${NAT_KEEP:-}" ]; then
        mkdir -p "$NAT_KEEP/$mode"
        cp "$work"/*.log "$work"/*.timeline "$NAT_KEEP/$mode/" 2>/dev/null || true
        chown -R "${SUDO_UID:-0}:${SUDO_GID:-0}" "$NAT_KEEP"
    fi
    rm -rf "$work"
    exit "$rc"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# Also clears what a killed earlier run left behind.
teardown() {
    for ns in "${NAMESPACES[@]}"; do
        ip netns pids "urmanat-$ns" 2>/dev/null | xargs -r kill -KILL || true
        ip netns del "urmanat-$ns" 2>/dev/null || true
    done
}

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
    # Unsolicited inbound to the WAN address is dropped, never answered with a
    # RST or port unreachable: a refused punch dial would end DCUtR at once.
    at "$1" iptables -A INPUT -i wan -m conntrack --ctstate NEW,INVALID -j DROP
}

topology() {
    teardown
    for ns in "${NAMESPACES[@]}"; do
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
    if [ "$mode" = cone-proxy ]; then
        at p iptables -t nat -A INPUT -i eth0 -p tcp --dport 28332 -j SNAT --to-source 172.16.0.1
    fi
    at a ping -c 1 -W 2 "$P_IP" >/dev/null
    at b ping -c 1 -W 2 "$P_IP" >/dev/null
}

# With A and B listening, the internet side routes straight at each LAN
# through its router: new inbound flows (ICMP or TCP to 28332) must die in
# FORWARD, and the WAN address must not answer on 28332 either.
sealed() {
    local lan wan pair
    for pair in "192.168.1.2 11.0.2.10" "192.168.2.2 11.0.3.10"; do
        read -r lan wan <<<"$pair"
        at inet ip route add "${lan%.*}.0/24" via "$wan"
        if at inet ping -c 1 -W 1 "$lan" >/dev/null 2>&1 ||
            at inet timeout 3 bash -c "exec 3<>/dev/tcp/$lan/28332" 2>/dev/null ||
            at inet timeout 3 bash -c "exec 3<>/dev/tcp/$wan/28332" 2>/dev/null; then
            echo "$lan is reachable from outside its NAT" >&2
            exit 1
        fi
        at inet ip route del "${lan%.*}.0/24"
    done
    echo "ok: A and B unreachable from outside their NATs"
}

# Chain sync is pinned to a closed local port: the fixture's headers are
# already selected and nothing leaves the topology.
serve() {
    local node=$1
    ip netns exec "urmanat-$node" env --default-signal=INT ${NAT_LOG:+RUST_LOG="$NAT_LOG"} "$bin/urma-node" --data "$work/$node/data" --config "$work/$node/config.json" \
        serve --listen 127.0.0.1:28331 --token-file "$work/$node/service.token" \
        --p2p --no-commit-public --chain-peer 127.0.0.1:9 \
        --peer-key "$work/$node/peer.key" --urma-listen 0.0.0.0:28332 --no-seeds "${@:2}" \
        >>"$work/$node.log" 2>&1 &
    pid[$node]=$!
    wait_for 30 "$node overlay up" up "$node"
    id[$node]=$(status "$node" | jq -r .overlay.peer_id)
}

up() { status "$1" | jq -e .overlay.peer_id >/dev/null; }

# Overlay state of every node every 2 s, for the diagnostics.
sample() {
    local node
    trap - EXIT INT TERM
    while :; do
        for node in "${NODES[@]}"; do
            status "$node" 2>/dev/null | jq -c --argjson t "$SECONDS" '{t: $t} + (.overlay | {
                reachability, relays: [.relays[]? | .[-6:]], connected,
                peers: [.peers[] | {id: .label[-6:], link, synced_through}],
                hole_punch: (.hole_punch | {attempts, successes, last_error}),
                book: .discovery.book, gossip: .discovery.gossip})' \
                >>"$work/$node.timeline" || true
        done
        sleep 2
    done
}

# wait_for SECONDS WHAT COMMAND...
wait_for() {
    local deadline=$((SECONDS + $1)) what=$2 n
    shift 2
    until "$@" >/dev/null 2>&1; do
        for n in "${!pid[@]}"; do
            kill -0 "${pid[$n]}" 2>/dev/null || { echo "$n exited: $what" >&2; return 1; }
        done
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

# NODE's book records a successful dial of PEER.
reached() {
    control "$1" overlay-book |
        jq -e --arg id "${id[$2]}" 'any(.[]; .peer_id == $id and .last_success != null)' >/dev/null
}

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

# NODE ran a punch and recorded its failure.
punch_failed() {
    status "$1" | jq -e '.overlay.hole_punch
        | .attempts >= 1 and .last_error != null and .last_error_at != null' >/dev/null
}

punched() {
    status "$1" | jq -e '.overlay.hole_punch | .attempts >= 1 and .successes >= 1' >/dev/null
}

stored() { status "$1" | jq .archive.stored_transactions; }

# NODE holds at least what PEER holds and its cursor on PEER covers the
# blocks PEER scanned: B gets A's last two blocks as data, A gets coverage.
synced_from() {
    local tip
    tip=$(status "$2" | jq .archive.indexed_through.height)
    [ "$(stored "$1")" -ge "$(stored "$2")" ] && [ "$(peer "$1" "$2" | jq .synced_through)" = "$tip" ]
}

either() { "$1" a || "$1" b; }

# A direct A-B link may be released once sync idles: latch every event
# the moment one poll sees it.
cone_events() {
    local node
    linked a b direct && seen[a-b]=1
    linked b a direct && seen[b-a]=1
    punched a && seen[a-punched]=1
    punched b && seen[b-punched]=1
    synced_from a b && seen[a-from-b]=1
    synced_from b a && seen[b-from-a]=1
    # Both sides count the punch that made the link direct, not a later one.
    if [ -n "${seen[a-b]:-}" ] && [ -n "${seen[b-a]:-}" ]; then
        linked_at=${linked_at:-$SECONDS}
        for node in a b; do
            if [ -z "${seen[$node-punched]:-}" ] && [ $((SECONDS - linked_at)) -gt 10 ]; then
                echo "$node did not count the punch that linked A and B" >&8
                exit 1
            fi
        done
    fi
    [ "${#seen[@]}" -eq 6 ]
}

successes() { status "$1" | jq '.overlay.hole_punch.successes'; }

# A restarted: its counters start over, B's must grow past `b_before`.
restart_events() {
    reserved a && again[reserved]=1
    linked a b direct && again[a-b]=1
    linked b a direct && again[b-a]=1
    punched a && again[a-punched]=1
    [ "$(successes b)" -gt "$b_before" ] && again[b-punched]=1
    [ "${#again[@]}" -eq 5 ]
}

# Stop A (SIGINT, or SIGKILL in cone-kill), start it again on the same key and archive.
restart_a() {
    b_before=$(successes b)
    kill -"$stop_signal" "${pid[a]}"
    for _ in $(seq 30); do
        kill -0 "${pid[a]}" 2>/dev/null || break
        sleep 0.5
    done
    if kill -0 "${pid[a]}" 2>/dev/null; then
        echo "A still running 15 s after SIG$stop_signal" >&2
        exit 1
    fi
    echo "ok: A stopped on SIG$stop_signal"
    serve a "${seeds[@]}"
}

# B's entry in NODE's book: no failure and no ban (clean), or one of them.
book_b() {
    control "$1" overlay-book | jq -e --arg id "${id[b]}" --arg want "$2" \
        'any(.[]; .peer_id == $id
            and ((.failures == 0 and .banned_until == null) == ($want == "clean")))' >/dev/null
}

# Held for 30 s: never a direct A-B link, no cursor either way, and each
# node holds what it scanned itself. Status peers are direct sync peers
# only, and P serves only blocks it scanned (none).
symmetric_hold() {
    local node other
    for _ in $(seq 15); do
        if linked a b direct || linked b a direct; then
            echo "direct A-B link through symmetric NAT" >&2
            return 1
        fi
        for node in a b; do
            other=$([ "$node" = a ] && echo b || echo a)
            if peer "$node" "$other" | jq -e '.synced_through != null' >/dev/null ||
                [ "$(stored "$node")" != "${base[$node]}" ]; then
                echo "$node synced from $other without a direct link" >&2
                return 1
            fi
        done
        sleep 2
    done
    echo "ok: no direct link, nothing synced over the relay"
}

topology
for node in "${NODES[@]}"; do
    mkdir -m 700 "$work/$node"
    cp -R --preserve=mode "$fixture/$node/." "$work/$node/"
done
serve p --urma-announce "/ip4/$P_IP/tcp/28332" --urma-announce "/ip4/$P_IP/udp/28332/quic-v1"
seeds=(--urma-seed "/ip4/$P_IP/tcp/28332/p2p/${id[p]}")
if [ "$mode" = cone-proxy ]; then
    seeds+=(--urma-seed "/ip4/$P_IP/udp/28332/quic-v1/p2p/${id[p]}")
fi
serve a "${seeds[@]}"
serve b "${seeds[@]}"
base[a]=$(stored a)
base[b]=$(stored b)
sample &
sampler=$!
sealed

wait_for 120 "A reaches P" reached a p
wait_for 120 "B reaches P" reached b p
wait_for 60 "P lists A and B" eval 'listed p a && listed p b'
wait_for 120 "P public, A and B private" \
    eval 'reachability p public && reachability a private && reachability b private'
wait_for 120 "A and B reserve on P" eval 'reserved a && reserved b'
# Reservations are what make A and B reachable: they must outlive idle.
for _ in $(seq 15); do
    sleep 2
    for node in a b; do
        reserved "$node" || { echo "$node dropped its reservation on P" >&2; exit 1; }
    done
done
echo "ok: A and B hold their reservations for 30 s"
wait_for 180 "A learns B's circuit address through P" circuit a b p
wait_for 60 "A-P and B-P links are direct" eval 'linked a p direct && linked b p direct'

case $mode in
cone | cone-proxy | cone-restart | cone-kill)
    wait_for 240 "direct A-B link, hole punch success on A and B, B synced A's records, A B's coverage" cone_events
    book_b a clean || { echo "A blames B in its book after the sync" >&2; exit 1; }
    echo "ok: B's entry in A's book is clean"
    if [ "$mode" = cone-restart ] || [ "$mode" = cone-kill ]; then
        restart_a
        wait_for 60 "restarted A reserves on P and punches a new direct link to B" restart_events
    fi
    ;;
symmetric)
    wait_for 180 "hole punch failure recorded" either punch_failed
    symmetric_hold
    wait_for 240 "B's entry in A's book shows a failure or backoff" book_b a failed
    ;;
esac
echo "nat $mode ok"
