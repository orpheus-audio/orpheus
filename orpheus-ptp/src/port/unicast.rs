//! Unicast negotiation (IEEE 1588-2019 16.1): the grants a master port
//! gives and the unicast messages it sends for them; a follower's
//! requests for unicast Sync and Delay_Resp from its master; and requests
//! for unicast Announce from known peers whose multicast doesn't arrive
//! (unicast discovery).
//!
//! A grant counts the messages it has left, and a request the ticks of a
//! timer, rather than the time, so that clock steps don't move their end.

use arrayvec::ArrayVec;

use super::{
    actions::TimestampContextInner, sequence_id::SequenceIdGenerator, state::PortState,
    Destination, PeerId, Port, PortAction, PortActionIterator, Running, Source, TimestampContext,
};
use crate::{
    datastructures::{
        common::{PortIdentity, TlvSetBuilder},
        messages::{Header, Message, MessageBody, MessageType, UnicastTlv, MAX_DATA_LEN},
    },
    filters::Filter,
    ptp_instance::PtpInstanceStateMutex,
    time::Interval,
};

/// Peers a port sends Sync or Announce to by unicast at most.
pub(super) const MAX_GRANTS: usize = 32;

/// The longest grant a port gives, in seconds.
const MAX_GRANT_S: u32 = 3600;

#[derive(Debug)]
struct Grant {
    peer: PeerId,
    message_type: MessageType,
    /// Sent with every `every`-th multicast message of its type.
    every: u32,
    phase: u32,
    /// Messages left before the grant runs out.
    left: u32,
    seq_ids: SequenceIdGenerator,
    due: bool,
}

#[derive(Debug, Default)]
pub(super) struct Grants {
    grants: ArrayVec<Grant, MAX_GRANTS>,
    signaling_seq_ids: SequenceIdGenerator,
}

impl Grants {
    /// Counts a multicast `message_type` going out; returns whether a
    /// unicast copy is due for any grant.
    pub(super) fn tick(&mut self, message_type: MessageType) -> bool {
        let mut due = false;
        for grant in self.grants.iter_mut() {
            if grant.message_type == message_type {
                grant.phase += 1;
                if grant.phase >= grant.every {
                    grant.phase = 0;
                    grant.due = true;
                }
            }
            due |= grant.due;
        }
        due
    }

    /// Drops all grants: the port is no longer master, and its peers ask
    /// their new one.
    pub(super) fn clear(&mut self) {
        if !self.grants.is_empty() {
            log::debug!("dropping {} unicast grants", self.grants.len());
        }
        self.grants.clear();
    }

    /// The next unicast message due, Sync first: its peer, type and
    /// sequence id.
    fn next_due(&mut self) -> Option<(PeerId, MessageType, u16)> {
        let index = self
            .grants
            .iter()
            .position(|g| g.due && g.message_type == MessageType::Sync)
            .or_else(|| self.grants.iter().position(|g| g.due))?;
        let grant = &mut self.grants[index];
        grant.due = false;
        grant.left -= 1;
        let next = (grant.peer, grant.message_type, grant.seq_ids.generate());
        if grant.left == 0 {
            log::debug!("unicast {:?} grant for {:?} ran out", next.1, next.0);
            self.grants.remove(index);
        }
        Some(next)
    }

    fn any_due(&self) -> bool {
        self.grants.iter().any(|g| g.due)
    }

    /// Grants or refuses `message_type` every `interval` for `duration_s` to
    /// `peer`, given the port's own interval for it. Renewing keeps the
    /// sequence ids going.
    fn request(
        &mut self,
        peer: PeerId,
        message_type: MessageType,
        interval: Interval,
        duration_s: u32,
        own: Interval,
    ) -> UnicastTlv {
        let refuse = UnicastTlv::Grant {
            message_type,
            interval,
            duration_s: 0,
            renewal_invited: false,
        };
        let steps = interval.as_log_2() as i32 - own.as_log_2() as i32;
        if duration_s == 0 || !(0..=16).contains(&steps) {
            return refuse;
        }
        let duration_s = duration_s.min(MAX_GRANT_S);
        let log = interval.as_log_2() as i32;
        let left = if log < 0 {
            duration_s << -log
        } else {
            (duration_s >> log).max(1)
        };
        let existing = self
            .grants
            .iter_mut()
            .find(|g| g.peer == peer && g.message_type == message_type);
        match existing {
            Some(grant) => {
                grant.every = 1 << steps;
                grant.left = left;
            }
            None => {
                let grant = Grant {
                    peer,
                    message_type,
                    every: 1 << steps,
                    phase: 0,
                    left,
                    seq_ids: SequenceIdGenerator::new(),
                    due: false,
                };
                if self.grants.try_push(grant).is_err() {
                    log::warn!("unicast grant table full, refusing {:?}", peer);
                    return refuse;
                }
                log::debug!(
                    "granted unicast {:?} every {:?} for {} s to {:?}",
                    message_type,
                    interval,
                    duration_s,
                    peer
                );
            }
        }
        UnicastTlv::Grant {
            message_type,
            interval,
            duration_s,
            renewal_invited: true,
        }
    }

    fn cancel(&mut self, peer: PeerId, message_type: MessageType) {
        self.grants
            .retain(|g| !(g.peer == peer && g.message_type == message_type));
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.grants.len()
    }

    /// The peers sent Sync to: one grant each.
    pub(super) fn followers(&self) -> usize {
        self.grants
            .iter()
            .filter(|g| g.message_type == MessageType::Sync)
            .count()
    }
}

impl<A, C, F: Filter, R, S: PtpInstanceStateMutex> Port<'_, Running, A, R, C, F, S> {
    /// Answers the unicast negotiation TLVs in a signaling message.
    pub(super) fn handle_signaling(
        &mut self,
        message: &Message<'_>,
        from: Source,
    ) -> PortActionIterator<'_> {
        let MessageBody::Signaling(signaling) = &message.body else {
            return actions![];
        };
        let target = signaling.target_port_identity;
        if from.multicast
            || !(target == self.port_identity
                || target.clock_identity == PortIdentity::WILDCARD.clock_identity)
        {
            return actions![];
        }

        let mut tlv_buffer = [0; MAX_DATA_LEN];
        let mut builder = TlvSetBuilder::new(&mut tlv_buffer);
        let mut margin = MAX_DATA_LEN - 34 - 10;
        for tlv in message.unicast_tlvs() {
            let answer = match tlv {
                UnicastTlv::Request {
                    message_type,
                    interval,
                    duration_s,
                } => {
                    // Announce only from a master: a port that doesn't send
                    // it by multicast has nothing to tell.
                    let master = matches!(self.port_state, PortState::Master);
                    let own = match message_type {
                        MessageType::Sync => Some(self.config.sync_interval),
                        MessageType::Announce if master => Some(self.config.announce_interval),
                        _ => None,
                    };
                    match (message_type, own) {
                        (_, Some(own)) => {
                            self.grants
                                .request(from.peer, message_type, interval, duration_s, own)
                        }
                        // Delay requests that come by unicast get their
                        // answer that way, granted or not.
                        (MessageType::DelayResp, None) if duration_s > 0 => UnicastTlv::Grant {
                            message_type,
                            interval,
                            duration_s: duration_s.min(MAX_GRANT_S),
                            renewal_invited: true,
                        },
                        _ => UnicastTlv::Grant {
                            message_type,
                            interval,
                            duration_s: 0,
                            renewal_invited: false,
                        },
                    }
                }
                UnicastTlv::Cancel { message_type, .. } => {
                    self.grants.cancel(from.peer, message_type);
                    self.requests.answer(from.peer, tlv);
                    UnicastTlv::AcknowledgeCancel {
                        message_type,
                        maintain_request: false,
                        maintain_grant: false,
                    }
                }
                UnicastTlv::Grant { .. } => {
                    self.requests.answer(from.peer, tlv);
                    continue;
                }
                UnicastTlv::AcknowledgeCancel { .. } => continue,
            };
            // A grant is the largest TLV: 12 bytes.
            if margin < 12 || answer.add_to(&mut builder).is_err() {
                break;
            }
            margin -= 12;
        }
        let tlvs = builder.build();
        if tlvs.wire_size() == 0 {
            return actions![];
        }

        let header = self.instance_state.with_ref(|state| Header {
            sdo_id: state.default_ds.sdo_id,
            domain_number: state.default_ds.domain_number,
            source_port_identity: self.port_identity,
            sequence_id: self.grants.signaling_seq_ids.generate(),
            ..Header::new(self.config.minor_ptp_version.into())
        });
        let answer = Message::signaling(header, message.header.source_port_identity, tlvs);
        let packet_length = match answer.serialize(&mut self.packet_buffer) {
            Ok(length) => length,
            Err(error) => {
                log::error!("Could not serialize signaling message: {:?}", error);
                return actions![];
            }
        };
        actions![PortAction::SendGeneral {
            data: &self.packet_buffer[..packet_length],
            link_local: false,
            to: Destination::Unicast(from.peer),
        }]
    }

    /// Sends the next unicast message its grants have due, and asks to be
    /// called again right away while more are.
    pub(super) fn send_unicast(&mut self) -> PortActionIterator<'_> {
        if !matches!(self.port_state, PortState::Master) {
            self.grants.clear();
            return actions![];
        }
        let Some((peer, message_type, id)) = self.grants.next_due() else {
            return actions![];
        };
        let minor = self.config.minor_ptp_version.into();
        let mut message = self.instance_state.with_ref(|state| match message_type {
            MessageType::Sync => Message::sync(&state.default_ds, self.port_identity, id, minor),
            _ => Message::announce(state, self.port_identity, id, minor),
        });
        message.header.unicast_flag = true;
        let packet_length = match message.serialize(&mut self.packet_buffer) {
            Ok(length) => length,
            Err(error) => {
                log::error!(
                    "Could not serialize unicast {:?}: {:?}",
                    message_type,
                    error
                );
                return actions![];
            }
        };
        let data = &self.packet_buffer[..packet_length];
        let to = Destination::Unicast(peer);
        let send = match message_type {
            MessageType::Sync => PortAction::SendEvent {
                context: TimestampContext {
                    inner: TimestampContextInner::Sync { id, to },
                },
                data,
                link_local: false,
                to,
            },
            _ => PortAction::SendGeneral {
                data,
                link_local: false,
                to,
            },
        };
        if self.grants.any_due() {
            actions![
                send,
                PortAction::ResetUnicastTimer {
                    duration: core::time::Duration::ZERO,
                }
            ]
        } else {
            actions![send]
        }
    }
}

/// How long a port asks for, in seconds.
const REQUEST_S: u32 = 60;

/// How long a port waits for an answer before it counts the request as
/// refused, in seconds.
const ANSWER_WAIT_S: u32 = 5;

/// How long a port waits before asking again after a refusal, in seconds.
const RETRY_S: u32 = 60;

/// How often a port repeats a renewal that got no answer, in seconds.
const RENEW_EVERY_S: u32 = 5;

/// Ticks without a unicast Sync, past which a follower no longer counts on
/// its grant: the master may have restarted and forgotten it.
const SYNC_LOST_S: u32 = 3;

/// Seconds without a multicast Announce from a known peer, after which
/// the port asks it for Announce by unicast: multicast doesn't get
/// through, for example because a firewall drops IGMP.
const DISCOVER_AFTER_S: u32 = 10;

/// How long a port waits before asking a peer for Announce again after a
/// refusal, in seconds: a peer that isn't master yet refuses, as do the
/// followers, which send no Announce.
const DISCOVER_RETRY_S: u32 = 30;

/// Peers a port can discover by unicast at most.
pub(super) const MAX_DISCOVER: usize = 16;

/// The negotiation tick, which counts the seconds of a port's requests.
const TICK: core::time::Duration = core::time::Duration::from_secs(1);

/// Where a follower's request for unicast Sync stands
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UnicastState {
    /// Not asked for: not wanted, or no master to ask.
    #[default]
    Off,
    /// Asked, no answer yet.
    Requested,
    /// The master sends Sync by unicast.
    Granted,
    /// The master refused or didn't answer; the follower asks again later
    /// and takes multicast meanwhile.
    Refused,
}

/// A request for one message type from one peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Stream {
    #[default]
    Idle,
    Requested {
        waited_s: u32,
    },
    Granted {
        left_s: u32,
        duration_s: u32,
    },
    Refused {
        retry_s: u32,
    },
}

impl Stream {
    /// Moves on by `elapsed_s`.
    fn elapse(&mut self, elapsed_s: u32) {
        *self = match *self {
            Stream::Idle => Stream::Idle,
            Stream::Requested { waited_s } if waited_s + elapsed_s >= ANSWER_WAIT_S => {
                log::info!("no answer to unicast request, using multicast");
                Stream::Refused { retry_s: RETRY_S }
            }
            Stream::Requested { waited_s } => Stream::Requested {
                waited_s: waited_s + elapsed_s,
            },
            Stream::Granted { left_s, .. } if left_s <= elapsed_s => Stream::Idle,
            Stream::Granted { left_s, duration_s } => Stream::Granted {
                left_s: left_s - elapsed_s,
                duration_s,
            },
            Stream::Refused { retry_s } if retry_s <= elapsed_s => Stream::Idle,
            Stream::Refused { retry_s } => Stream::Refused {
                retry_s: retry_s - elapsed_s,
            },
        }
    }

    /// Whether to ask (again) now: nothing asked yet, or a grant is
    /// halfway through and not renewed yet.
    fn due(&self) -> bool {
        match *self {
            Stream::Idle => true,
            Stream::Granted { left_s, duration_s } => {
                left_s <= duration_s / 2 && (duration_s / 2 - left_s) % RENEW_EVERY_S == 0
            }
            _ => false,
        }
    }

    /// Notes that the request went out.
    fn ask(&mut self) {
        if *self == Stream::Idle {
            *self = Stream::Requested { waited_s: 0 };
        }
    }

    fn granted(&self) -> bool {
        matches!(self, Stream::Granted { .. })
    }

    fn asked(&self) -> bool {
        matches!(self, Stream::Requested { .. } | Stream::Granted { .. })
    }

    /// Takes the answer.
    fn answer(&mut self, message_type: MessageType, duration_s: u32) {
        if duration_s == 0 {
            let retry_s = if message_type == MessageType::Announce {
                log::debug!("unicast Announce refused");
                DISCOVER_RETRY_S
            } else {
                log::info!("unicast {:?} refused", message_type);
                RETRY_S
            };
            *self = Stream::Refused { retry_s };
        } else {
            if !self.granted() {
                log::info!("unicast {:?} granted for {} s", message_type, duration_s);
            }
            *self = Stream::Granted {
                left_s: duration_s,
                duration_s,
            };
        }
    }
}

/// A peer the port should hear Announce from, by multicast or else by
/// unicast.
#[derive(Debug, Clone, Copy)]
struct Known {
    peer: PeerId,
    /// Seconds since its last multicast Announce.
    quiet_s: u32,
    announce: Stream,
}

/// A port's requests for unicast: Sync and Delay_Resp from its master,
/// and Announce from known peers whose multicast doesn't arrive.
#[derive(Debug, Default)]
pub(super) struct Requests {
    want: bool,
    /// The master asked, and how it is reached.
    master: Option<(PortIdentity, PeerId)>,
    /// How the current master was last heard from.
    parent_peer: Option<(PortIdentity, PeerId)>,
    sync: Stream,
    delay_resp: Stream,
    /// Seconds since the last unicast Sync.
    quiet_s: u32,
    known: ArrayVec<Known, MAX_DISCOVER>,
    /// Where discovery looks first in the next tick, so that every peer
    /// gets its turn.
    next_known: usize,
    /// The negotiation tick has started.
    ticking: bool,
}

impl Requests {
    pub(super) fn set_want(&mut self, want: bool) {
        self.want = want;
    }

    /// Sets the peers to discover; those already known keep their state.
    pub(super) fn set_known(&mut self, peers: &[PeerId]) {
        let mut known = ArrayVec::new();
        for &peer in peers.iter().take(MAX_DISCOVER) {
            known.push(
                self.known
                    .iter()
                    .find(|k| k.peer == peer)
                    .copied()
                    .unwrap_or(Known {
                        peer,
                        quiet_s: 0,
                        announce: Stream::Idle,
                    }),
            );
        }
        self.known = known;
    }

    pub(super) fn state(&self) -> UnicastState {
        match self.sync {
            _ if self.master.is_none() => UnicastState::Off,
            Stream::Idle => UnicastState::Off,
            Stream::Requested { .. } => UnicastState::Requested,
            Stream::Granted { .. } => UnicastState::Granted,
            Stream::Refused { .. } => UnicastState::Refused,
        }
    }

    /// Notes a message from the port's master `sender`: how it reaches
    /// the port, and whether a unicast Sync came.
    pub(super) fn heard(&mut self, sender: PortIdentity, from: Source, sync: bool) {
        self.parent_peer = Some((sender, from.peer));
        if sync && !from.multicast && self.master.is_some_and(|(m, _)| m == sender) {
            self.quiet_s = 0;
        }
    }

    /// Notes an Announce from `from`.
    pub(super) fn heard_announce(&mut self, from: Source) {
        if let Some(known) = self.known.iter_mut().find(|k| k.peer == from.peer) {
            if from.multicast {
                known.quiet_s = 0;
            }
        }
    }

    /// How `master` reaches the port, once heard.
    pub(super) fn peer_of(&self, master: PortIdentity) -> Option<PeerId> {
        self.parent_peer
            .filter(|(sender, _)| *sender == master)
            .map(|(_, peer)| peer)
    }

    /// Starts over with a new master, still wanting what was wanted and
    /// knowing whom it knew.
    pub(super) fn reset(&mut self) {
        *self = Requests {
            want: self.want,
            known: core::mem::take(&mut self.known),
            next_known: self.next_known,
            ticking: self.ticking,
            ..Default::default()
        };
    }

    /// Whether to drop multicast Sync and Follow_Up from `sender`: it sends
    /// them by unicast.
    pub(super) fn drops_multicast_from(&self, sender: PortIdentity) -> bool {
        self.sync.granted() && self.master.is_some_and(|(m, _)| m == sender)
    }

    /// Where Delay_Req goes.
    pub(super) fn delay_req_to(&self) -> Destination {
        match self.master {
            Some((_, peer)) if self.delay_resp.granted() => Destination::Unicast(peer),
            _ => Destination::Multicast,
        }
    }

    /// Whether `peer` reaches the port only by unicast: its Announce
    /// comes that way.
    fn discovered(&self, peer: PeerId) -> bool {
        self.known
            .iter()
            .any(|k| k.peer == peer && k.announce.granted())
    }

    /// Takes a grant or cancel from `from`.
    fn answer(&mut self, from: PeerId, tlv: UnicastTlv) {
        let (message_type, duration_s) = match tlv {
            UnicastTlv::Grant {
                message_type,
                duration_s,
                ..
            } => (message_type, duration_s),
            UnicastTlv::Cancel { message_type, .. } => (message_type, 0),
            _ => return,
        };
        if message_type == MessageType::Announce {
            if let Some(known) = self.known.iter_mut().find(|k| k.peer == from) {
                known.announce.answer(message_type, duration_s);
            }
            return;
        }
        if self.master.is_none_or(|(_, peer)| peer != from) {
            return;
        }
        match message_type {
            MessageType::Sync => {
                if duration_s > 0 && !self.sync.granted() {
                    self.quiet_s = 0;
                }
                self.sync.answer(message_type, duration_s)
            }
            MessageType::DelayResp => self.delay_resp.answer(message_type, duration_s),
            _ => {}
        }
    }

    /// One tick of the requests to the master `parent`, if following one:
    /// what to send to whom.
    /// `intervals`: those to ask for Sync and Delay_Resp at.
    fn tick_master(
        &mut self,
        parent: Option<PortIdentity>,
        elapsed_s: u32,
        intervals: (Interval, Interval),
        tlvs: &mut ArrayVec<UnicastTlv, 2>,
    ) -> Option<(PortIdentity, PeerId)> {
        // Unicast is wanted, or the only way the master is heard.
        let parent_peer = parent.and_then(|p| self.peer_of(p));
        let want = self.want || parent_peer.is_some_and(|p| self.discovered(p));
        if let Some((master, peer)) = self.master {
            if Some(master) != parent || !want {
                for (message_type, stream) in [
                    (MessageType::Sync, self.sync),
                    (MessageType::DelayResp, self.delay_resp),
                ] {
                    if stream.asked() {
                        tlvs.push(UnicastTlv::Cancel {
                            message_type,
                            maintain_request: false,
                            maintain_grant: false,
                        });
                    }
                }
                self.reset();
                return Some((master, peer));
            }
        } else if want {
            self.master = parent.zip(parent_peer);
        }
        let (master, peer) = self.master?;

        self.sync.elapse(elapsed_s);
        self.delay_resp.elapse(elapsed_s);
        if self.sync.granted() {
            self.quiet_s += elapsed_s;
            if self.quiet_s > SYNC_LOST_S {
                log::info!("unicast Sync stopped, asking again");
                self.sync = Stream::Idle;
            }
        }
        for (message_type, stream, interval) in [
            (MessageType::Sync, &mut self.sync, intervals.0),
            (MessageType::DelayResp, &mut self.delay_resp, intervals.1),
        ] {
            if stream.due() {
                stream.ask();
                tlvs.push(UnicastTlv::Request {
                    message_type,
                    interval,
                    duration_s: REQUEST_S,
                });
            }
        }
        Some((master, peer))
    }

    /// One tick of discovery: the peer to ask for Announce now, if any.
    fn tick_known(&mut self, elapsed_s: u32, ask: bool) -> Option<PeerId> {
        for known in self.known.iter_mut() {
            known.quiet_s = known.quiet_s.saturating_add(elapsed_s);
            known.announce.elapse(elapsed_s);
            if known.quiet_s < DISCOVER_AFTER_S && !known.announce.asked() {
                // Multicast arrives: nothing to ask for, or to renew.
                known.announce = Stream::Idle;
            }
        }
        if !ask || self.known.is_empty() {
            return None;
        }
        let n = self.known.len();
        let start = self.next_known % n;
        let index = (0..n).map(|i| (start + i) % n).find(|&i| {
            let k = &self.known[i];
            k.quiet_s >= DISCOVER_AFTER_S && k.announce.due()
        })?;
        self.next_known = index + 1;
        let known = &mut self.known[index];
        known.announce.ask();
        Some(known.peer)
    }
}

impl<A, C, F: Filter, R, S: PtpInstanceStateMutex> Port<'_, Running, A, R, C, F, S> {
    /// The negotiation tick: asks the master for Sync and Delay_Resp,
    /// renews and cancels, and asks known peers for Announce whose
    /// multicast doesn't arrive.
    pub(super) fn tick_requests(&mut self) -> PortActionIterator<'_> {
        let intervals = (
            self.config.sync_interval,
            self.config.min_delay_req_interval(),
        );
        let requests = &mut self.requests;
        let elapsed_s = if requests.ticking { 1 } else { 0 };
        requests.ticking = true;
        let tick = PortAction::ResetNegotiationTimer { duration: TICK };

        let parent = match &self.port_state {
            PortState::Slave(state) => Some(state.remote_master),
            _ => None,
        };
        let mut tlvs: ArrayVec<UnicastTlv, 2> = ArrayVec::new();
        let mut to = requests.tick_master(parent, elapsed_s, intervals, &mut tlvs);
        let discover = requests.tick_known(elapsed_s, tlvs.is_empty());
        if let Some(peer) = discover {
            tlvs.push(UnicastTlv::Request {
                message_type: MessageType::Announce,
                interval: self.config.announce_interval,
                duration_s: REQUEST_S,
            });
            to = Some((PortIdentity::WILDCARD, peer));
        }

        let Some((target, peer)) = to.filter(|_| !tlvs.is_empty()) else {
            return actions![tick];
        };
        let mut tlv_buffer = [0; 32];
        let mut builder = TlvSetBuilder::new(&mut tlv_buffer);
        for tlv in &tlvs {
            // Two TLVs of at most 12 bytes fit.
            tlv.add_to(&mut builder).unwrap();
        }
        let header = self.instance_state.with_ref(|state| Header {
            sdo_id: state.default_ds.sdo_id,
            domain_number: state.default_ds.domain_number,
            source_port_identity: self.port_identity,
            sequence_id: self.grants.signaling_seq_ids.generate(),
            ..Header::new(self.config.minor_ptp_version.into())
        });
        let message = Message::signaling(header, target, builder.build());
        let packet_length = match message.serialize(&mut self.packet_buffer) {
            Ok(length) => length,
            Err(error) => {
                log::error!("Could not serialize signaling message: {:?}", error);
                return actions![tick];
            }
        };
        actions![
            tick,
            PortAction::SendGeneral {
                data: &self.packet_buffer[..packet_length],
                link_local: false,
                to: Destination::Unicast(peer),
            }
        ]
    }
}

#[cfg(test)]
mod tests {
    use std::vec::Vec;

    use super::*;
    use core::cell::RefCell;

    use crate::{
        config::AcceptAnyMaster,
        datastructures::{
            common::ClockIdentity,
            messages::{DelayReqMessage, SyncMessage},
        },
        filters::BasicFilter,
        port::{
            state::{SlaveState, SyncState},
            tests::{setup_test_port, setup_test_state, TestClock},
            NoForwardedTLVs,
        },
        time::Time,
        PtpInstanceState,
    };

    const PEER: PeerId = PeerId(7);
    const FROM: Source = Source {
        peer: PEER,
        multicast: false,
    };
    const REQUESTER: PortIdentity = PortIdentity {
        clock_identity: ClockIdentity([9; 8]),
        port_number: 1,
    };

    fn request(tlvs: &[UnicastTlv], buffer: &mut [u8]) -> usize {
        let mut tlv_buffer = [0; 128];
        let mut builder = TlvSetBuilder::new(&mut tlv_buffer);
        for tlv in tlvs {
            tlv.add_to(&mut builder).unwrap();
        }
        let header = Header {
            source_port_identity: REQUESTER,
            ..Header::new(1)
        };
        Message::signaling(header, PortIdentity::WILDCARD, builder.build())
            .serialize(buffer)
            .unwrap()
    }

    fn req(message_type: MessageType, log: i8, duration_s: u32) -> UnicastTlv {
        UnicastTlv::Request {
            message_type,
            interval: Interval::from_log_2(log),
            duration_s,
        }
    }

    /// The unicast TLVs of a signaling message sent to `PEER`.
    fn answer(mut actions: PortActionIterator<'_>) -> Vec<UnicastTlv> {
        let Some(PortAction::SendGeneral {
            data,
            to: Destination::Unicast(PEER),
            ..
        }) = actions.next()
        else {
            panic!("no answer");
        };
        assert!(actions.next().is_none());
        let message = Message::deserialize(data).unwrap();
        assert!(message.header.unicast_flag);
        let MessageBody::Signaling(signaling) = &message.body else {
            panic!("not signaling");
        };
        assert_eq!(signaling.target_port_identity, REQUESTER);
        message.unicast_tlvs().collect()
    }

    fn grant(message_type: MessageType, log: i8, duration_s: u32) -> UnicastTlv {
        UnicastTlv::Grant {
            message_type,
            interval: Interval::from_log_2(log),
            duration_s,
            renewal_invited: duration_s > 0,
        }
    }

    #[test]
    fn grants_what_it_can() {
        let state = setup_test_state();
        let mut port = setup_test_port(&state);
        port.set_forced_port_state(PortState::Master);
        // The test port syncs every 1 s and announces every 2 s.
        let mut buffer = [0; 256];
        let n = request(
            &[
                req(MessageType::Sync, 0, 60),
                req(MessageType::Announce, 2, 60),
                req(MessageType::DelayResp, 0, 60),
                req(MessageType::PDelayResp, 0, 60),
                req(MessageType::Sync, -1, 60),
            ],
            &mut buffer,
        );
        let tlvs = answer(port.handle_general_receive(&buffer[..n], FROM));
        assert_eq!(
            tlvs,
            [
                grant(MessageType::Sync, 0, 60),
                grant(MessageType::Announce, 2, 60),
                grant(MessageType::DelayResp, 0, 60),
                // No peer delay, nor Sync faster than multicast.
                grant(MessageType::PDelayResp, 0, 0),
                grant(MessageType::Sync, -1, 0),
            ]
        );
        assert_eq!(port.grants.len(), 2);
        assert_eq!(port.unicast_followers(), 1);

        // Announce only from a master.
        port.set_forced_port_state(PortState::Listening);
        let n = request(&[req(MessageType::Announce, 1, 60)], &mut buffer);
        let tlvs = answer(port.handle_general_receive(&buffer[..n], FROM));
        assert_eq!(tlvs, [grant(MessageType::Announce, 1, 0)]);

        // Not by multicast.
        let n = request(&[req(MessageType::Sync, 0, 60)], &mut buffer);
        let multicast = Source {
            multicast: true,
            ..FROM
        };
        assert!(port
            .handle_general_receive(&buffer[..n], multicast)
            .next()
            .is_none());
    }

    #[test]
    fn sends_unicast_sync_until_the_grant_runs_out() {
        let state = setup_test_state();
        let mut port = setup_test_port(&state);
        port.set_forced_port_state(PortState::Master);
        let mut buffer = [0; 256];
        let n = request(&[req(MessageType::Sync, 0, 2)], &mut buffer);
        let _ = port.handle_general_receive(&buffer[..n], FROM).count();

        for round in 0..3 {
            let actions: Vec<_> = port.handle_sync_timer().map(kind).collect();
            if round == 2 {
                // Two messages of 1 s each: done.
                assert_eq!(actions, ["timer", "event multicast"]);
                assert!(port.handle_unicast_timer().next().is_none());
                break;
            }
            assert_eq!(actions, ["timer", "event multicast", "unicast timer"]);

            let mut actions = port.handle_unicast_timer();
            let Some(PortAction::SendEvent {
                context,
                data,
                to: Destination::Unicast(PEER),
                ..
            }) = actions.next()
            else {
                panic!("no unicast sync");
            };
            let message = Message::deserialize(data).unwrap();
            assert!(matches!(message.body, MessageBody::Sync(_)));
            assert!(message.header.unicast_flag);
            assert_eq!(message.header.sequence_id, round);
            assert!(actions.next().is_none());
            drop(actions);

            let mut actions = port.handle_send_timestamp(context, Time::from_micros(5));
            let Some(PortAction::SendGeneral {
                data,
                to: Destination::Unicast(PEER),
                ..
            }) = actions.next()
            else {
                panic!("no unicast follow up");
            };
            let message = Message::deserialize(data).unwrap();
            assert!(matches!(message.body, MessageBody::FollowUp(_)));
            assert_eq!(message.header.sequence_id, round);
        }
        assert_eq!(port.grants.len(), 0);
    }

    #[test]
    fn slower_grants_skip_multicast_rounds() {
        let state = setup_test_state();
        let mut port = setup_test_port(&state);
        port.set_forced_port_state(PortState::Master);
        let mut buffer = [0; 256];
        let n = request(&[req(MessageType::Announce, 2, 60)], &mut buffer);
        let _ = port.handle_general_receive(&buffer[..n], FROM).count();
        // Every 4 s against multicast every 2 s.
        let due: Vec<bool> = (0..4)
            .map(|_| {
                let unicast = port
                    .handle_announce_timer(&mut NoForwardedTLVs)
                    .any(|a| matches!(a, PortAction::ResetUnicastTimer { .. }));
                if unicast {
                    let mut actions = port.handle_unicast_timer();
                    assert!(matches!(
                        actions.next(),
                        Some(PortAction::SendGeneral {
                            to: Destination::Unicast(PEER),
                            ..
                        })
                    ));
                }
                unicast
            })
            .collect();
        assert_eq!(due, [false, true, false, true]);
    }

    #[test]
    fn cancel_and_leaving_master_end_grants() {
        let state = setup_test_state();
        let mut port = setup_test_port(&state);
        port.set_forced_port_state(PortState::Master);
        let mut buffer = [0; 256];
        let n = request(
            &[
                req(MessageType::Sync, 0, 60),
                req(MessageType::Announce, 1, 60),
            ],
            &mut buffer,
        );
        let _ = port.handle_general_receive(&buffer[..n], FROM).count();
        assert_eq!(port.grants.len(), 2);

        let cancel = UnicastTlv::Cancel {
            message_type: MessageType::Sync,
            maintain_request: false,
            maintain_grant: false,
        };
        let n = request(&[cancel], &mut buffer);
        let tlvs = answer(port.handle_general_receive(&buffer[..n], FROM));
        assert_eq!(
            tlvs,
            [UnicastTlv::AcknowledgeCancel {
                message_type: MessageType::Sync,
                maintain_request: false,
                maintain_grant: false,
            }]
        );
        assert_eq!(port.grants.len(), 1);

        port.set_forced_port_state(PortState::Listening);
        assert_eq!(port.grants.len(), 0);
    }

    #[test]
    fn answers_unicast_delay_requests_by_unicast() {
        let state = setup_test_state();
        let mut port = setup_test_port(&state);
        port.set_forced_port_state(PortState::Master);
        let header = Header {
            source_port_identity: REQUESTER,
            ..Header::new(1)
        };
        let message = Message {
            header,
            body: MessageBody::DelayReq(DelayReqMessage {
                origin_timestamp: Default::default(),
            }),
            suffix: Default::default(),
        };
        let mut buffer = [0; 128];
        let n = message.serialize(&mut buffer).unwrap();
        for (from, to) in [
            (FROM, Destination::Unicast(PEER)),
            (
                Source {
                    multicast: true,
                    ..FROM
                },
                Destination::Multicast,
            ),
        ] {
            let mut actions = port.handle_event_receive(&buffer[..n], Time::from_micros(1), from);
            let Some(PortAction::SendGeneral { data, to: sent, .. }) = actions.next() else {
                panic!("no delay response");
            };
            assert_eq!(sent, to);
            let message = Message::deserialize(data).unwrap();
            assert_eq!(message.header.unicast_flag, !from.multicast);
        }
    }

    const GM: PortIdentity = PortIdentity {
        clock_identity: ClockIdentity([3; 8]),
        port_number: 1,
    };
    const GM_PEER: PeerId = PeerId(3);
    const GM_UNICAST: Source = Source {
        peer: GM_PEER,
        multicast: false,
    };
    const GM_MULTICAST: Source = Source {
        peer: GM_PEER,
        multicast: true,
    };

    fn sync_from_gm(buffer: &mut [u8]) -> usize {
        Message {
            header: Header {
                source_port_identity: GM,
                two_step_flag: true,
                ..Header::new(1)
            },
            body: MessageBody::Sync(SyncMessage {
                origin_timestamp: Default::default(),
            }),
            suffix: Default::default(),
        }
        .serialize(buffer)
        .unwrap()
    }

    fn grants_from_gm(grants: &[UnicastTlv], buffer: &mut [u8]) -> usize {
        let mut tlv_buffer = [0; 64];
        let mut builder = TlvSetBuilder::new(&mut tlv_buffer);
        for tlv in grants {
            tlv.add_to(&mut builder).unwrap();
        }
        let header = Header {
            source_port_identity: GM,
            ..Header::new(1)
        };
        Message::signaling(header, PortIdentity::default(), builder.build())
            .serialize(buffer)
            .unwrap()
    }

    /// The unicast TLVs a follower sent to the master, if any.
    fn sent_to_gm(actions: PortActionIterator<'_>) -> Option<Vec<UnicastTlv>> {
        let mut sent = None;
        for action in actions {
            if let PortAction::SendGeneral {
                data,
                to: Destination::Unicast(GM_PEER),
                ..
            } = action
            {
                let message = Message::deserialize(data).unwrap();
                let MessageBody::Signaling(signaling) = &message.body else {
                    panic!("not signaling");
                };
                assert_eq!(signaling.target_port_identity, GM);
                sent = Some(message.unicast_tlvs().collect());
            }
        }
        sent
    }

    type TestPort<'a> =
        Port<'a, Running, AcceptAnyMaster, rand::rngs::mock::StepRng, TestClock, BasicFilter>;

    fn follower(state: &RefCell<PtpInstanceState>) -> TestPort<'_> {
        let mut port = setup_test_port(state);
        port.set_forced_port_state(PortState::Slave(SlaveState::new(GM)));
        port.set_unicast(true);
        let mut buffer = [0; 128];
        let n = sync_from_gm(&mut buffer);
        let _ = port
            .handle_event_receive(&buffer[..n], Time::from_micros(1), GM_MULTICAST)
            .count();
        port
    }

    fn requests() -> [UnicastTlv; 2] {
        [
            req(MessageType::Sync, 0, 60),
            req(MessageType::DelayResp, 1, 60),
        ]
    }

    fn sync_state(port: &TestPort<'_>) -> bool {
        matches!(
            &port.port_state,
            PortState::Slave(SlaveState {
                sync_state: SyncState::Measuring { .. },
                ..
            })
        )
    }

    #[test]
    fn follower_asks_and_takes_unicast() {
        let state = setup_test_state();
        let mut port = follower(&state);
        assert_eq!(port.unicast_state(), UnicastState::Off);
        assert_eq!(
            sent_to_gm(port.handle_negotiation_timer()),
            Some(requests().into())
        );
        assert_eq!(port.unicast_state(), UnicastState::Requested);

        let mut buffer = [0; 128];
        let n = grants_from_gm(
            &[
                grant(MessageType::Sync, 0, 60),
                grant(MessageType::DelayResp, 1, 60),
            ],
            &mut buffer,
        );
        assert!(port
            .handle_general_receive(&buffer[..n], GM_UNICAST)
            .next()
            .is_none());
        assert_eq!(port.unicast_state(), UnicastState::Granted);

        // Multicast Sync from the master is dropped now, unicast taken.
        port.set_forced_port_state(PortState::Slave(SlaveState::new(GM)));
        port.requests.master = Some((GM, GM_PEER));
        port.requests.sync.answer(MessageType::Sync, 60);
        port.requests.delay_resp.answer(MessageType::DelayResp, 60);
        let n = sync_from_gm(&mut buffer);
        let _ = port
            .handle_event_receive(&buffer[..n], Time::from_micros(2), GM_MULTICAST)
            .count();
        assert!(!sync_state(&port));
        let _ = port
            .handle_event_receive(&buffer[..n], Time::from_micros(3), GM_UNICAST)
            .count();
        assert!(sync_state(&port));

        // Delay requests go by unicast.
        let mut actions = port.handle_delay_request_timer();
        let to = actions.find_map(|a| match a {
            PortAction::SendEvent { to, data, .. } => {
                assert!(Message::deserialize(data).unwrap().header.unicast_flag);
                Some(to)
            }
            _ => None,
        });
        assert_eq!(to, Some(Destination::Unicast(GM_PEER)));
    }

    #[test]
    fn follower_renews_and_notices_lost_grants() {
        let state = setup_test_state();
        let mut port = follower(&state);
        let _ = port.handle_negotiation_timer().count();
        let mut buffer = [0; 128];
        let n = grants_from_gm(
            &[
                grant(MessageType::Sync, 0, 10),
                grant(MessageType::DelayResp, 1, 10),
            ],
            &mut buffer,
        );
        let _ = port
            .handle_general_receive(&buffer[..n], GM_UNICAST)
            .count();
        let sync = sync_from_gm(&mut buffer);
        let mut asked = Vec::new();
        for second in 1..=8 {
            // Unicast Sync keeps coming for the first three seconds.
            if second <= 3 {
                let _ = port
                    .handle_event_receive(&buffer[..sync], Time::from_micros(second), GM_UNICAST)
                    .count();
            }
            if let Some(tlvs) = sent_to_gm(port.handle_negotiation_timer()) {
                asked.push((second, tlvs));
            }
        }
        assert_eq!(
            asked,
            [
                // Halfway through the 10 s: renew both.
                (5, requests().into()),
                // Four ticks without unicast Sync: ask again.
                (6, [req(MessageType::Sync, 0, 60)].into()),
            ]
        );
        assert_eq!(port.unicast_state(), UnicastState::Requested);
    }

    #[test]
    fn follower_falls_back_to_multicast_without_answer() {
        let state = setup_test_state();
        let mut port = follower(&state);
        let _ = port.handle_negotiation_timer().count();
        for _ in 0..ANSWER_WAIT_S {
            assert_eq!(sent_to_gm(port.handle_negotiation_timer()), None);
        }
        assert_eq!(port.unicast_state(), UnicastState::Refused);
        // Asks again a minute later.
        let asked = (0..RETRY_S)
            .filter(|_| sent_to_gm(port.handle_negotiation_timer()).is_some())
            .count();
        assert_eq!(asked, 1);
        assert_eq!(port.unicast_state(), UnicastState::Requested);
    }

    #[test]
    fn follower_cancels_when_no_longer_wanted() {
        let state = setup_test_state();
        let mut port = follower(&state);
        let _ = port.handle_negotiation_timer().count();
        let mut buffer = [0; 128];
        let n = grants_from_gm(&[grant(MessageType::Sync, 0, 60)], &mut buffer);
        let _ = port
            .handle_general_receive(&buffer[..n], GM_UNICAST)
            .count();
        port.set_unicast(false);
        let cancel = |message_type| UnicastTlv::Cancel {
            message_type,
            maintain_request: false,
            maintain_grant: false,
        };
        assert_eq!(
            sent_to_gm(port.handle_negotiation_timer()),
            // Delay_Resp was asked for, unanswered.
            Some([cancel(MessageType::Sync), cancel(MessageType::DelayResp)].into())
        );
        assert_eq!(port.unicast_state(), UnicastState::Off);
        assert_eq!(sent_to_gm(port.handle_negotiation_timer()), None);
    }

    /// The peers asked for Announce in each of `ticks` ticks.
    fn discovery(port: &mut TestPort<'_>, ticks: u32) -> Vec<(u32, PeerId)> {
        let mut asked = Vec::new();
        for tick in 0..ticks {
            for action in port.handle_negotiation_timer() {
                if let PortAction::SendGeneral {
                    data,
                    to: Destination::Unicast(peer),
                    ..
                } = action
                {
                    let message = Message::deserialize(data).unwrap();
                    let tlvs: Vec<_> = message.unicast_tlvs().collect();
                    if tlvs == [req(MessageType::Announce, 1, 60)] {
                        let MessageBody::Signaling(signaling) = &message.body else {
                            panic!("not signaling");
                        };
                        assert_eq!(signaling.target_port_identity, PortIdentity::WILDCARD);
                        asked.push((tick, peer));
                    }
                }
            }
        }
        asked
    }

    #[test]
    fn discovers_peers_whose_multicast_is_missing() {
        let state = setup_test_state();
        let mut port = setup_test_port(&state);
        let (heard, missing) = (PeerId(10), PeerId(11));
        port.set_unicast_peers(&[heard, missing]);
        let mut asked = Vec::new();
        for second in 0..13 {
            port.requests.heard_announce(Source {
                peer: heard,
                multicast: true,
            });
            asked.extend(
                discovery(&mut port, 1)
                    .into_iter()
                    .map(|(_, p)| (second, p)),
            );
        }
        // After 10 s without its multicast, only the missing one.
        assert_eq!(asked, [(10, missing)]);

        // Granted: renewed halfway, and it counts as discovered.
        port.requests
            .answer(missing, grant(MessageType::Announce, 1, 20));
        assert!(port.requests.discovered(missing));
        let renewals: Vec<u32> = discovery(&mut port, 16)
            .into_iter()
            .filter(|(_, p)| *p == missing)
            .map(|(t, _)| t)
            .collect();
        assert_eq!(renewals, [9, 14]);
    }

    #[test]
    fn discovery_takes_turns() {
        let state = setup_test_state();
        let mut port = setup_test_port(&state);
        port.set_unicast_peers(&[PeerId(1), PeerId(2), PeerId(3)]);
        let asked: Vec<PeerId> = discovery(&mut port, 14)
            .into_iter()
            .map(|(_, p)| p)
            .collect();
        assert_eq!(asked, [PeerId(1), PeerId(2), PeerId(3)]);
    }

    #[test]
    fn follower_asks_a_master_heard_only_by_unicast() {
        let state = setup_test_state();
        let mut port = setup_test_port(&state);
        port.set_unicast_peers(&[GM_PEER]);
        port.set_forced_port_state(PortState::Slave(SlaveState::new(GM)));
        port.requests.heard(GM, GM_UNICAST, false);
        // Not wanted, and multicast from the master arrives: no request.
        let _ = port.handle_negotiation_timer().count();
        assert_eq!(port.unicast_state(), UnicastState::Off);
        // Its Announce comes by unicast only: Sync and Delay_Resp too.
        port.requests
            .answer(GM_PEER, grant(MessageType::Announce, 1, 60));
        assert_eq!(
            sent_to_gm(port.handle_negotiation_timer()),
            Some(requests().into())
        );
    }

    fn kind(action: PortAction<'_>) -> &'static str {
        match action {
            PortAction::SendEvent {
                to: Destination::Multicast,
                ..
            } => "event multicast",
            PortAction::SendEvent { .. } => "event unicast",
            PortAction::ResetUnicastTimer { .. } => "unicast timer",
            PortAction::ResetSyncTimer { .. } => "timer",
            _ => "other",
        }
    }
}
