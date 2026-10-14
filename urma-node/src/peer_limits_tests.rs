use super::*;
use libp2p::swarm::{
    DialError, ListenError,
    behaviour::{ConnectionClosed, ConnectionEstablished, DialFailure, ListenFailure},
};

fn id(value: usize) -> ConnectionId {
    ConnectionId::new_unchecked(value)
}

fn address() -> Multiaddr {
    "/ip4/127.0.0.1/tcp/1234".parse().unwrap()
}

fn endpoint(direction: Direction) -> ConnectedPoint {
    match direction {
        Direction::Inbound => ConnectedPoint::Listener {
            local_addr: address(),
            send_back_addr: address(),
        },
        Direction::Outbound => ConnectedPoint::Dialer {
            address: address(),
            role_override: Endpoint::Dialer,
            port_use: PortUse::New,
        },
    }
}

fn begin(
    limits: &mut Limits,
    id: ConnectionId,
    direction: Direction,
) -> Result<(), ConnectionDenied> {
    match direction {
        Direction::Inbound => limits.handle_pending_inbound_connection(id, &address(), &address()),
        Direction::Outbound => limits
            .handle_pending_outbound_connection(id, None, &[], Endpoint::Dialer)
            .map(|_| ()),
    }
}

fn admit(
    limits: &mut Limits,
    id: ConnectionId,
    peer: PeerId,
    direction: Direction,
) -> Result<(), ConnectionDenied> {
    match direction {
        Direction::Inbound => {
            limits.handle_established_inbound_connection(id, peer, &address(), &address())
        }
        Direction::Outbound => limits.handle_established_outbound_connection(
            id,
            peer,
            &address(),
            Endpoint::Dialer,
            PortUse::Reuse,
        ),
    }
    .map(|_| ())
}

fn class(limits: &Limits, id: ConnectionId) -> Class {
    limits.classes.of(id).unwrap()
}

fn circuit() -> Multiaddr {
    let relay = PeerId::random();
    format!("/ip4/1.2.3.4/tcp/1/p2p/{relay}/p2p-circuit")
        .parse()
        .unwrap()
}

fn established(limits: &mut Limits, id: ConnectionId, peer: PeerId, direction: Direction) {
    limits.on_swarm_event(FromSwarm::ConnectionEstablished(ConnectionEstablished {
        peer_id: peer,
        connection_id: id,
        endpoint: &endpoint(direction),
        failed_addresses: &[],
        other_established: 0,
    }));
}

fn close(limits: &mut Limits, id: ConnectionId, peer: PeerId, direction: Direction) {
    limits.on_swarm_event(FromSwarm::ConnectionClosed(ConnectionClosed {
        peer_id: peer,
        connection_id: id,
        endpoint: &endpoint(direction),
        cause: None,
        remaining_established: 0,
    }));
}

fn failed(limits: &mut Limits, id: ConnectionId, direction: Direction, denied_elsewhere: bool) {
    match direction {
        Direction::Inbound => {
            let error = if denied_elsewhere {
                ListenError::Denied {
                    cause: denied(DialLimit::Pending),
                }
            } else {
                ListenError::Aborted
            };
            limits.on_swarm_event(FromSwarm::ListenFailure(ListenFailure {
                local_addr: &address(),
                send_back_addr: &address(),
                error: &error,
                connection_id: id,
                peer_id: None,
            }));
        }
        Direction::Outbound => {
            let error = if denied_elsewhere {
                DialError::Denied {
                    cause: denied(DialLimit::Pending),
                }
            } else {
                DialError::Aborted
            };
            limits.on_swarm_event(FromSwarm::DialFailure(DialFailure {
                peer_id: None,
                error: &error,
                connection_id: id,
            }));
        }
    }
}

fn empty(limits: &Limits) {
    assert!(limits.pending_in.is_empty());
    assert!(limits.pending_out.is_empty());
    assert!(limits.established.is_empty());
}

#[test]
fn pending_limits_are_independent_and_duplicate_callbacks_do_not_fill_slots() {
    let mut limits = Limits::default();
    for value in 0..4 {
        begin(&mut limits, id(value), Direction::Inbound).unwrap();
        begin(&mut limits, id(value + 4), Direction::Outbound).unwrap();
    }
    begin(&mut limits, id(0), Direction::Inbound).unwrap();
    begin(&mut limits, id(4), Direction::Outbound).unwrap();
    assert!(begin(&mut limits, id(8), Direction::Inbound).is_err());
    assert!(begin(&mut limits, id(9), Direction::Outbound).is_err());
    assert_eq!(limits.pending_in.len(), 4);
    assert_eq!(limits.pending_out.len(), 4);
    for value in 0..8 {
        let direction = if value < 4 {
            Direction::Inbound
        } else {
            Direction::Outbound
        };
        failed(&mut limits, id(value), direction, false);
    }
    empty(&limits);
}

#[test]
fn established_direction_limits_allow_eight_each_and_sixteen_total() {
    let mut limits = Limits::default();
    for (direction, offset) in [(Direction::Inbound, 0), (Direction::Outbound, 20)] {
        for value in 0..ESTABLISHED_PER_DIRECTION {
            let id = id(offset + value);
            let peer = PeerId::random();
            begin(&mut limits, id, direction).unwrap();
            admit(&mut limits, id, peer, direction).unwrap();
            established(&mut limits, id, peer, direction);
        }
        let denied_id = id(offset + ESTABLISHED_PER_DIRECTION);
        begin(&mut limits, denied_id, direction).unwrap();
        assert!(admit(&mut limits, denied_id, PeerId::random(), direction).is_err());
        assert!(!limits.pending_in.contains(&denied_id));
        assert!(!limits.pending_out.contains(&denied_id));
    }
    assert_eq!(limits.established.len(), ESTABLISHED_TOTAL);
    for direction in [Direction::Inbound, Direction::Outbound] {
        assert!(admit(&mut limits, id(99), PeerId::random(), direction).is_err());
    }
}

#[test]
fn a_peer_has_one_data_connection_across_both_directions() {
    for direction in [Direction::Inbound, Direction::Outbound] {
        let mut limits = Limits::default();
        let peer = PeerId::random();
        let opposite = if direction == Direction::Inbound {
            Direction::Outbound
        } else {
            Direction::Inbound
        };
        begin(&mut limits, id(1), direction).unwrap();
        admit(&mut limits, id(1), peer, direction).unwrap();
        established(&mut limits, id(1), peer, direction);
        assert_eq!(class(&limits, id(1)), Class::Data);
        begin(&mut limits, id(2), Direction::Outbound).unwrap();
        assert!(admit(&mut limits, id(2), peer, Direction::Outbound).is_err());
        assert!(limits.pending_in.is_empty());
        assert!(limits.pending_out.is_empty());
        close(&mut limits, id(1), peer, direction);
        begin(&mut limits, id(3), opposite).unwrap();
        admit(&mut limits, id(3), peer, opposite).unwrap();
        assert_eq!(class(&limits, id(3)), Class::Data);
    }
}

#[test]
fn a_direct_peer_gets_one_auxiliary_and_one_relayed_connection_besides_data() {
    let mut limits = Limits::default();
    let peer = PeerId::random();
    admit(&mut limits, id(1), peer, Direction::Outbound).unwrap();
    established(&mut limits, id(1), peer, Direction::Outbound);
    // An AutoNAT dial-back or a crossing punch dial: inbound, or a fresh port.
    admit(&mut limits, id(2), peer, Direction::Inbound).unwrap();
    established(&mut limits, id(2), peer, Direction::Inbound);
    assert_eq!(class(&limits, id(2)), Class::Auxiliary);
    let elsewhere: Multiaddr = "/ip4/127.0.0.1/tcp/4321".parse().unwrap();
    let second = limits.handle_established_inbound_connection(id(3), peer, &address(), &elsewhere);
    assert!(second.is_err(), "one auxiliary per peer");
    let fresh = limits.handle_established_outbound_connection(
        id(3),
        peer,
        &address(),
        Endpoint::Dialer,
        PortUse::New,
    );
    assert!(fresh.is_err(), "the auxiliary slot is taken");
    close(&mut limits, id(2), peer, Direction::Inbound);
    limits
        .handle_established_outbound_connection(
            id(3),
            peer,
            &address(),
            Endpoint::Dialer,
            PortUse::New,
        )
        .unwrap();
    assert_eq!(class(&limits, id(3)), Class::Auxiliary);
    let relayed = |limits: &mut Limits, id| {
        limits.handle_established_inbound_connection(id, peer, &circuit(), &address())
    };
    relayed(&mut limits, id(4)).unwrap();
    established(&mut limits, id(4), peer, Direction::Inbound);
    assert_eq!(class(&limits, id(4)), Class::Relayed);
    assert!(relayed(&mut limits, id(5)).is_err());
    assert_eq!(limits.established.len(), 2, "id(3) awaits establishment");
    assert!(
        limits.closing.is_empty(),
        "no link came from a reused address"
    );
}

#[test]
fn a_peer_redialing_from_the_same_address_replaces_its_stale_links_within_budget() {
    let mut limits = Limits::default();
    let peer = PeerId::random();
    admit(&mut limits, id(1), peer, Direction::Inbound).unwrap();
    established(&mut limits, id(1), peer, Direction::Inbound);
    for value in 1..ESTABLISHED_PER_DIRECTION {
        let other = PeerId::random();
        admit(&mut limits, id(10 + value), other, Direction::Inbound).unwrap();
        established(&mut limits, id(10 + value), other, Direction::Inbound);
    }
    assert!(admit(&mut limits, id(30), PeerId::random(), Direction::Inbound).is_err());
    let elsewhere: Multiaddr = "/ip4/127.0.0.1/tcp/4321".parse().unwrap();
    limits
        .handle_established_inbound_connection(id(2), peer, &address(), &elsewhere)
        .unwrap();
    assert_eq!(
        class(&limits, id(2)),
        Class::Auxiliary,
        "a second connection from another address (an AutoNAT dial-back) replaces nothing"
    );
    assert!(limits.closing.is_empty());
    limits.forget(id(2));
    admit(&mut limits, id(3), peer, Direction::Inbound).unwrap();
    established(&mut limits, id(3), peer, Direction::Inbound);
    assert_eq!(
        class(&limits, id(3)),
        Class::Data,
        "the redial takes the stale link's data slot"
    );
    assert_eq!(limits.closing, vec![(peer, id(1))]);
    let inbound_data = limits.held(id(99), PeerId::random(), Direction::Inbound);
    assert_eq!(inbound_data.data_direction, ESTABLISHED_PER_DIRECTION);
    assert!(admit(&mut limits, id(31), PeerId::random(), Direction::Inbound).is_err());
    admit(&mut limits, id(3), peer, Direction::Inbound).unwrap();
    assert_eq!(
        limits.closing,
        vec![(peer, id(1))],
        "a link never replaces itself"
    );
    close(&mut limits, id(1), peer, Direction::Inbound);
    assert!(limits.retiring.is_empty());
    assert_eq!(limits.established.len(), ESTABLISHED_PER_DIRECTION);
}

#[test]
fn reservation_holders_pass_the_data_budget_only_while_the_relay_is_open() {
    let relay_open = Arc::new(AtomicBool::new(false));
    let mut limits = Limits::new(Classes::default(), Arc::clone(&relay_open));
    for value in 0..ESTABLISHED_PER_DIRECTION {
        let peer = PeerId::random();
        admit(&mut limits, id(value), peer, Direction::Inbound).unwrap();
        established(&mut limits, id(value), peer, Direction::Inbound);
    }
    assert!(admit(&mut limits, id(50), PeerId::random(), Direction::Inbound).is_err());
    relay_open.store(true, Ordering::Release);
    admit(&mut limits, id(50), PeerId::random(), Direction::Inbound).unwrap();
    assert_eq!(class(&limits, id(50)), Class::Holder);
    for value in 0..ESTABLISHED_PER_DIRECTION {
        let peer = PeerId::random();
        admit(&mut limits, id(20 + value), peer, Direction::Outbound).unwrap();
        established(&mut limits, id(20 + value), peer, Direction::Outbound);
    }
    let outbound = admit(&mut limits, id(51), PeerId::random(), Direction::Outbound);
    assert!(outbound.is_err(), "we never dial past the data budget");
}

#[test]
fn a_demoted_connection_frees_its_data_slot_and_takes_one_back_only_when_free() {
    let mut limits = Limits::default();
    let kept = PeerId::random();
    admit(&mut limits, id(0), kept, Direction::Outbound).unwrap();
    established(&mut limits, id(0), kept, Direction::Outbound);
    for value in 1..ESTABLISHED_PER_DIRECTION {
        let peer = PeerId::random();
        admit(&mut limits, id(value), peer, Direction::Outbound).unwrap();
        established(&mut limits, id(value), peer, Direction::Outbound);
    }
    assert!(admit(&mut limits, id(20), PeerId::random(), Direction::Outbound).is_err());
    assert!(limits.demote(id(0)));
    assert!(!limits.demote(id(0)), "only a data connection is demoted");
    assert_eq!(class(&limits, id(0)), Class::Demoted);
    let newcomer = PeerId::random();
    admit(&mut limits, id(20), newcomer, Direction::Outbound).unwrap();
    established(&mut limits, id(20), newcomer, Direction::Outbound);
    assert_eq!(
        class(&limits, id(20)),
        Class::Data,
        "the freed slot is reused"
    );
    assert!(!limits.promote(id(0)), "no data slot is free");
    close(&mut limits, id(20), newcomer, Direction::Outbound);
    assert!(limits.promote(id(0)));
    assert_eq!(class(&limits, id(0)), Class::Data);
    assert!(
        !limits.promote(id(0)),
        "only a demoted connection is promoted"
    );
}

#[test]
fn demotion_is_capped_by_the_holder_budget_and_inbound_never_returns() {
    let relay_open = Arc::new(AtomicBool::new(true));
    let mut limits = Limits::new(Classes::default(), relay_open);
    for value in 0..HOLDERS_MAX {
        let (id, peer) = (id(1000 + value), PeerId::random());
        limits
            .established
            .insert(id, (peer, Direction::Inbound, Class::Holder));
    }
    let peer = PeerId::random();
    admit(&mut limits, id(1), peer, Direction::Inbound).unwrap();
    established(&mut limits, id(1), peer, Direction::Inbound);
    assert!(!limits.demote(id(1)), "the holder budget is spent");
    assert_eq!(class(&limits, id(1)), Class::Data);
    limits.forget(id(1000));
    assert!(limits.demote(id(1)));
    assert!(
        !limits.promote(id(1)),
        "an inbound demoted link never takes a data slot back"
    );
}

#[test]
fn a_demoted_peer_is_still_direct_for_admission() {
    let mut limits = Limits::default();
    let peer = PeerId::random();
    admit(&mut limits, id(1), peer, Direction::Outbound).unwrap();
    established(&mut limits, id(1), peer, Direction::Outbound);
    assert!(limits.demote(id(1)));
    assert!(
        admit(&mut limits, id(2), peer, Direction::Outbound).is_err(),
        "a second direct dial is a duplicate, not a new data connection"
    );
    admit(&mut limits, id(3), peer, Direction::Inbound).unwrap();
    assert_eq!(class(&limits, id(3)), Class::Auxiliary);
}

#[test]
fn failed_handshakes_release_pending_slots_in_both_directions() {
    for direction in [Direction::Inbound, Direction::Outbound] {
        let mut limits = Limits::default();
        for value in 0..4 {
            begin(&mut limits, id(value), direction).unwrap();
        }
        failed(&mut limits, id(0), direction, false);
        begin(&mut limits, id(4), direction).unwrap();
        for value in 1..5 {
            failed(&mut limits, id(value), direction, false);
        }
        empty(&limits);
    }
}

#[test]
fn downstream_denial_releases_state_without_counting_establishment() {
    for direction in [Direction::Inbound, Direction::Outbound] {
        let mut limits = Limits::default();
        for after_admission in [false, true] {
            begin(&mut limits, id(1), direction).unwrap();
            if after_admission {
                admit(&mut limits, id(1), PeerId::random(), direction).unwrap();
            }
            assert!(limits.established.is_empty());
            failed(&mut limits, id(1), direction, true);
            empty(&limits);
        }
    }
}

#[test]
fn distinct_peer_churn_retains_no_peer_history() {
    let mut limits = Limits::default();
    for value in 0..1000 {
        let direction = if value % 2 == 0 {
            Direction::Inbound
        } else {
            Direction::Outbound
        };
        let peer = PeerId::random();
        begin(&mut limits, id(value), direction).unwrap();
        admit(&mut limits, id(value), peer, direction).unwrap();
        established(&mut limits, id(value), peer, direction);
        close(&mut limits, id(value), peer, direction);
        empty(&limits);
        assert!(limits.inbound.is_empty());
    }
}

#[test]
fn duplicate_established_events_and_redundant_cleanup_are_harmless() {
    let mut limits = Limits::default();
    let peer = PeerId::random();
    begin(&mut limits, id(1), Direction::Inbound).unwrap();
    admit(&mut limits, id(1), peer, Direction::Inbound).unwrap();
    established(&mut limits, id(1), peer, Direction::Inbound);
    established(&mut limits, id(1), peer, Direction::Inbound);
    admit(&mut limits, id(1), peer, Direction::Inbound).unwrap();
    assert_eq!(limits.established.len(), 1);
    assert!(limits.closing.is_empty(), "a link never replaces itself");
    close(&mut limits, id(1), peer, Direction::Inbound);
    close(&mut limits, id(1), peer, Direction::Inbound);
    failed(&mut limits, id(1), Direction::Inbound, false);
    failed(&mut limits, id(1), Direction::Outbound, false);
    empty(&limits);
}
