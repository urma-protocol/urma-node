//! Soft release: what sync, an idle slot or a stalled request asks of a peer's
//! connections. The peer loses its data slot, unblamed: data connections are
//! demoted into the holder budget (closed outright once it is spent), so they
//! are neither live nor counted. A demoted link then lives only while it
//! carries a relay reservation either way or a circuit, or, while this node
//! still seeks reachability or relays, is one of the few public peers it
//! dialed; `Reach` retires every other one after the holder grace. That grace
//! outlasts an AutoNAT dial-back, which behind a NAT that drops unsolicited
//! packets takes longer than the sync idle window. A data connection whose
//! peer never proves it speaks this protocol on our chain is released the same
//! way. A demoted link we dialed whose peer did prove it takes a data slot
//! back once one is free; inbound slots go to newcomers. Misbehaviour still
//! disconnects outright.
use crate::{
    peer_gossip::Shared, peer_limits::Class, peer_reach::Remotes, peer_transport::Behaviour,
};
use libp2p::{PeerId, Swarm};
use std::time::Duration;

/// How long a data connection may stay open without the peer proving its
/// chain, and how long a demoted connection waits before taking a slot back.
const UNPROVEN: Duration = Duration::from_secs(60);
const DEMOTED: Duration = Duration::from_secs(60);

pub(crate) fn release(
    (swarm, remotes): (&mut Swarm<Behaviour>, &mut Remotes),
    shared: &Shared,
    peer: PeerId,
) {
    for (id, outbound) in remotes.data_of(peer) {
        if swarm.behaviour_mut().limits.demote(id) {
            remotes.reclass(id, Class::Demoted);
            shared.reclassed(peer, outbound, (Class::Data, Class::Demoted));
            tracing::debug!(%peer, "released peer demoted");
        } else {
            tracing::debug!(%peer, "holder budget spent; released link closed");
            swarm.close_connection(id);
        }
    }
}

/// Run on every tick of the network loop.
pub(crate) fn maintain((swarm, remotes): (&mut Swarm<Behaviour>, &mut Remotes), shared: &Shared) {
    for peer in remotes.unproven(UNPROVEN, |peer| shared.is_on_chain(peer)) {
        tracing::debug!(%peer, "data connection never proved its chain; released");
        release((swarm, remotes), shared, peer);
    }
    for (id, peer, outbound) in remotes.demoted_after(DEMOTED) {
        if shared.is_on_chain(peer) && swarm.behaviour_mut().limits.promote(id) {
            remotes.reclass(id, Class::Data);
            shared.reclassed(peer, outbound, (Class::Demoted, Class::Data));
        }
    }
}
