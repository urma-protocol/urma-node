# U5 nat-core WIP (delete before the lot lands)

Contract: /home/tragdate/ROSINT/private/docs/urma/URMA_NODE_NAT.md (§3, §3.6a binding).
Interface commit 8543288 (reported to U1). Gate: cargo test --locked -p urma-node --no-fail-fast.

## Done (uncompiled after 8543288)
- Cargo: libp2p features + quic autonat relay dcutr; direct dep `either`.
- peer_limits.rs: connection classes Data / Auxiliary / Holder / Relayed, shared
  `Classes` map (ConnectionId -> Class), `Limits::new(classes, relay_open)`.
  Data: 1 per peer, 8 per direction, 16 total (unchanged). Relayed: 1 per peer, 8 total.
  Auxiliary: second direct conn of a peer, only inbound or outbound PortUse::New
  (AutoNAT dial-backs, crossing punch dials), 1 per peer, 8 total.
  Holder: inbound past the data budget while relay_open, 128.
  Unit tests in that file NOT yet updated (a_peer_has_one_connection... now gets Auxiliary for inbound dup).
- peer_fence.rs: `Fenced<B>` wrapper. data_only = records/5 handler only on Data
  connections (hard gate §3.6a.3, both directions; inner never sees other conns).
  without_candidates = identify's NewExternalAddrCandidate dropped (§3.6a.4).
  `Candidates` behaviour: network loop offers accepted observed addrs; re-emits
  known set TCP first then QUIC so DCUtR (LRU, most recent first) tries QUIC first.
- peer_gossip.rs Shared: opened/closed(peer, outbound, Class); Links.data;
  has_direct = holds a Data connection.
- peer_transport.rs: rewritten. TCP+QUIC (QUIC: 8 streams, 256 KiB windows, MTU cap
  1280), DNS, relay client transport; Behaviour { limits, records: Fenced, identify:
  Fenced, candidates, autonat (v2 client), autonat_server, relay, relay_client, dcutr };
  `Wiring { classes, relay_open }`; swarm(identity, mode, &wiring). Relay config
  §3.3 with libp2p off-by-one (per-peer max 0 => 1 reservation, 3 => 4 circuits),
  `RelayGate` RateLimiter: open && peer != self. yamux streams 4 -> 8.

## Next
1. peer.rs: create Wiring (Classes + relay_open) in run(); `records.inner_mut()` for
   send_request/send_response; Remotes store peer + class; Shared::opened with class
   from `wiring.classes.of(id)`; QUIC listener derived from the TCP listen addr on
   the port TCP actually bound (same port, also for port 0); identify Received:
   skip relayed conns, only outbound direct, public, shape ip/(tcp|udp/quic-v1) ->
   `candidates.offer`; keep shared.observe for observed_addrs but NO add_external_address.
   peer.rs is ~720 lines: put NAT handling in a new peer_reach.rs (Reach state + Remotes).
2. peer_reach.rs: reachability = forced | Public if announce or any non-circuit external
   confirmed | Private if an AutoNAT probe failed | Unknown; set shared.nat + relay_open.
   ExternalAddrConfirmed/Expired: direct -> nat.set_external, circuit -> ignore.
   Reservation manager: when Private keep <=2 circuit listeners on outbound direct
   public-IP peers (oldest ConnectionId first ~ seeds first), backoff 10 min on
   ListenerClosed Err; drop circuits when not Private. relay server events ->
   nat.set_reserved_by; relay client ReservationReqAccepted -> nat.set_relay.
   NewListenAddr with /p2p-circuit -> nat.set_circuit, not shared.listening.
   dcutr Ok -> punch_succeeded + close relayed conns of that peer; Err -> punch_failed.
3. validate_announce: accept /ip4|ip6/x/udp/p/quic-v1 (own shape check; don't call
   U4's validate_gossip_address). U4 owns validate_dial/validate_gossip_address/check_gossip/peerbook.
4. Test knob: start_with (DnsMode + forced Reachability) replacing start_with_dns
   (only caller tests/peer_dns.rs).
5. Tests (netns like tests/common/discovery_host.rs; NAT experiments LOCAL, kernel has
   veth/nf_nat, sudo -n works): QUIC connect; AutoNAT public (listener on PUBLIC[0]
   dials P) / private (no listener); relay reservation + circuit, records refused on
   relayed, link=relayed; DCUtR success with forced-Private B on PUBLIC[0] (relayed
   closed after punch); limits unit tests updated.
6. Report to U1: Peers gossip cannot run over relayed links (records/5 hard gate) ->
   U4 must not Peers-probe circuit dials; autonat server may dial private addrs
   (no addr filter hook) residual; hole-punch retry backoff = U4's book backoff.
