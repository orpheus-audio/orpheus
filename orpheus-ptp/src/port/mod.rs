//! Abstraction of a network [`Port`] of a device.
//!
//! See [`Port`] for a detailed description.

use core::{cell::RefCell, ops::ControlFlow};

pub use actions::{
    Destination, ForwardedTLV, ForwardedTLVProvider, NoForwardedTLVs, PeerId, PortAction,
    PortActionIterator, Source, TimestampContext,
};
pub use measurement::Measurement;
use rand::Rng;
use state::PortState;
pub use unicast::UnicastState;

use self::sequence_id::SequenceIdGenerator;
pub use crate::datastructures::messages::{
    is_compatible as is_message_buffer_compatible, MAX_DATA_LEN,
};
#[cfg(doc)]
use crate::PtpInstance;
use crate::{
    bmc::{
        acceptable_master::AcceptableMasterList,
        bmca::{BestAnnounceMessage, Bmca},
    },
    clock::Clock,
    config::PortConfig,
    datastructures::{
        common::PortIdentity,
        messages::{Message, MessageBody},
    },
    filters::{Filter, FilterEstimate},
    observability::{self, port::PortDS},
    ptp_instance::{PtpInstanceState, PtpInstanceStateMutex},
    time::{Duration, Time},
};

// Needs to be here because of use rules
macro_rules! actions {
    [] => {
        {
            crate::port::PortActionIterator::from(::arrayvec::ArrayVec::new())
        }
    };
    [$action:expr] => {
        {
            let mut list = ::arrayvec::ArrayVec::new();
            list.push($action);
            crate::port::PortActionIterator::from(list)
        }
    };
    [$($action:expr),+] => {
        {
            let mut list = ::arrayvec::ArrayVec::new();
            $(list.push($action);)+
            crate::port::PortActionIterator::from(list)
        }
    };
}

mod actions;
mod bmca;
mod master;
mod measurement;
mod sequence_id;
mod slave;
pub(crate) mod state;
mod unicast;

/// A single port of the PTP instance
///
/// One of these needs to be created per port of the PTP instance. They are
/// created by calling [`PtpInstance::add_port`].
///
/// # Generics
/// A [`Port`] is generic over:
/// * **`L`**: The state of the `Port`, either [`InBmca`], or [`Running`].
/// * **`A`**: The type of the [`PortConfig::acceptable_master_list`] which
///   should implement [`AcceptableMasterList`]
/// * **`R`**: The type of the random number generator ([`Rng`]) used to
///   randomize timing
/// * **`C`**: The type of the [`Clock`] used by this [`Port`]
/// * **`F`**: The type of the [`Filter`] used by this [`Port`]
///
///
/// ## Type States
/// A [`Port`] can be in two states. Either in [`Running`] allowing access to
/// the [`handle_*`](`Port::handle_send_timestamp`) methods. Or in [`InBmca`]
/// state where it can be used with a [`PtpInstance`](`crate::PtpInstance`) to
/// run the best master clock algotithm (BMCA).
///
/// To transition from [`InBmca`] to [`Running`] use [`Port::end_bmca`]. To
/// transition from [`Running`] to [`InBmca`] use [`Port::start_bmca`].
///
/// # Example
///
/// ## Initialization
/// A [`Port`] can be created from a [`PtpInstance`]. It requires a
/// [`PortConfig`], a [`Filter::Config`], a [`Clock`], and a [`Rng`].
///
/// ```no_run
/// # mod system {
/// #     pub struct Clock;
/// #     impl orpheus_ptp::Clock for Clock {
/// #         type Error = ();
/// #         fn now(&self) -> orpheus_ptp::time::Time {
/// #             unimplemented!()
/// #         }
/// #         fn step_clock(&mut self, _: orpheus_ptp::time::Duration) -> Result<orpheus_ptp::time::Time, Self::Error> {
/// #             unimplemented!()
/// #         }
/// #         fn set_frequency(&mut self, _: f64) -> Result<orpheus_ptp::time::Time, Self::Error> {
/// #             unimplemented!()
/// #         }
/// #         fn set_properties(&mut self, _: &orpheus_ptp::config::TimePropertiesDS) -> Result<(), Self::Error> {
/// #             unimplemented!()
/// #         }
/// #     }
/// # }
/// # let (instance_config, time_properties_ds) = unimplemented!();
/// use rand::thread_rng;
/// use orpheus_ptp::config::{AcceptAnyMaster, DelayMechanism, PortConfig, PtpMinorVersion};
/// use orpheus_ptp::filters::BasicFilter;
/// use orpheus_ptp::PtpInstance;
/// use orpheus_ptp::time::Interval;
///
/// let mut instance = PtpInstance::<BasicFilter>::new(instance_config, time_properties_ds);
///
/// // TODO make these values sensible
/// let interval = Interval::from_log_2(-2); // 2^(-2)s = 250ms
/// let port_config = PortConfig {
///     acceptable_master_list: AcceptAnyMaster,
///     delay_mechanism: DelayMechanism::E2E { interval },
///     announce_interval: interval,
///     announce_receipt_timeout: 0,
///     sync_interval: interval,
///     master_only: false,
///     delay_asymmetry: Default::default(),
///     minor_ptp_version: PtpMinorVersion::One,
/// };
/// let filter_config = 1.0;
/// let clock = system::Clock {};
/// let rng = thread_rng();
///
/// let port_in_bmca = instance.add_port(port_config, filter_config, clock, rng);
///
/// // To handle events for the port it needs to change to running mode
/// let (running_port, actions) = port_in_bmca.end_bmca();
///
/// // This returns the first actions that need to be handled for the port
/// # fn handle_actions<T>(_:T){}
/// handle_actions(actions);
///
/// # running_port.start_bmca(); // Make sure we check `A` bound
/// ```
///
/// ## Handling actions
/// The [`Port`] informs the user about any actions it needs the user to handle
/// via [`PortAction`]s returned from its methods. The user is responsible for
/// handling these events in their system specific way.
///
/// ```no_run
/// use orpheus_ptp::port::{PortAction, PortActionIterator, TimestampContext};
/// use orpheus_ptp::time::Time;
///
/// # mod system {
/// #     pub struct Timer;
/// #     impl Timer {
/// #         pub fn expire_in(&mut self, time: core::time::Duration) {}
/// #     }
/// #     pub struct UdpSocket;
/// #     impl UdpSocket {
/// #         pub fn send(&mut self, buf: &[u8], link_local: bool) -> orpheus_ptp::time::Time { unimplemented!() }
/// #     }
/// # }
/// struct MyPortResources {
///     announce_timer: system::Timer,
///     sync_timer: system::Timer,
///     delay_req_timer: system::Timer,
///     announce_receipt_timer: system::Timer,
///     filter_update_timer: system::Timer,
///     unicast_timer: system::Timer,
///     negotiation_timer: system::Timer,
///     time_critical_socket: system::UdpSocket,
///     general_socket: system::UdpSocket,
///     send_timestamp: Option<(TimestampContext, Time)>
/// }
///
/// fn handle_actions(resources: &mut MyPortResources, actions: PortActionIterator) {
///     for action in actions {
///         match action {
///             PortAction::SendEvent { context, data, link_local, to: _ } => {
///                 let timestamp = resources.time_critical_socket.send(data, link_local);
///                 resources.send_timestamp = Some((context, timestamp));
///             }
///             PortAction::SendGeneral { data, link_local, to: _ } => {
///                 resources.general_socket.send(data, link_local);
///             }
///             PortAction::ResetAnnounceTimer { duration } => {
///                 resources.announce_timer.expire_in(duration)
///             }
///             PortAction::ResetSyncTimer { duration } => resources.sync_timer.expire_in(duration),
///             PortAction::ResetDelayRequestTimer { duration } => {
///                 resources.delay_req_timer.expire_in(duration)
///             }
///             PortAction::ResetAnnounceReceiptTimer { duration } => {
///                 resources.announce_receipt_timer.expire_in(duration)
///             }
///             PortAction::ResetFilterUpdateTimer { duration } => {
///                 resources.filter_update_timer.expire_in(duration)
///             }
///             PortAction::ResetUnicastTimer { duration } => {
///                 resources.unicast_timer.expire_in(duration)
///             }
///             PortAction::ResetNegotiationTimer { duration } => {
///                 resources.negotiation_timer.expire_in(duration)
///             }
///             PortAction::ForwardTLV { .. } => {}
///         }
///     }
/// }
/// ```
///
/// ## Handling system events
/// After the initialization the user has to inform the [`Port`] about any
/// events relevant to it via the [`handle_*`](`Port::handle_send_timestamp`)
/// methods.
///
/// ```no_run
/// # mod system {
/// #    pub struct Timer;
/// #    impl Timer {
/// #        pub fn has_expired(&self) -> bool { true }
/// #    }
/// #    pub struct UdpSocket;
/// #    impl UdpSocket {
/// #         pub fn recv(&mut self) -> Option<(&'static [u8], orpheus_ptp::time::Time, orpheus_ptp::port::Source)> { unimplemented!() }
/// #    }
/// # }
/// # struct MyPortResources {
/// #     announce_timer: system::Timer,
/// #     sync_timer: system::Timer,
/// #     delay_req_timer: system::Timer,
/// #     announce_receipt_timer: system::Timer,
/// #     filter_update_timer: system::Timer,
/// #     unicast_timer: system::Timer,
/// #     negotiation_timer: system::Timer,
/// #     time_critical_socket: system::UdpSocket,
/// #     general_socket: system::UdpSocket,
/// #     send_timestamp: Option<(orpheus_ptp::port::TimestampContext, orpheus_ptp::time::Time)>
/// # }
///
/// use rand::Rng;
/// use orpheus_ptp::Clock;
/// use orpheus_ptp::config::AcceptableMasterList;
/// use orpheus_ptp::filters::Filter;
/// use orpheus_ptp::port::{NoForwardedTLVs, Port, PortActionIterator, Running};
///
/// fn something_happend(resources: &mut MyPortResources, running_port: &mut Port<Running, impl AcceptableMasterList, impl Rng, impl Clock, impl Filter>) {
///     let actions = if resources.announce_timer.has_expired() {
///         running_port.handle_announce_timer(&mut NoForwardedTLVs)
///     } else if resources.sync_timer.has_expired() {
///         running_port.handle_sync_timer()
///     } else if resources.delay_req_timer.has_expired() {
///         running_port.handle_delay_request_timer()
///     } else if resources.announce_receipt_timer.has_expired() {
///         running_port.handle_announce_receipt_timer()
///     } else if resources.filter_update_timer.has_expired() {
///         running_port.handle_filter_update_timer()
///     } else if resources.unicast_timer.has_expired() {
///         running_port.handle_unicast_timer()
///     } else if resources.negotiation_timer.has_expired() {
///         running_port.handle_negotiation_timer()
///     } else if let Some((data, timestamp, from)) = resources.time_critical_socket.recv() {
///         running_port.handle_event_receive(data, timestamp, from)
///     } else if let Some((data, _timestamp, from)) = resources.general_socket.recv() {
///         running_port.handle_general_receive(data, from)
///     } else if let Some((context, timestamp)) = resources.send_timestamp.take() {
///         running_port.handle_send_timestamp(context, timestamp)
///     } else {
///         PortActionIterator::empty()
///     };
///
/// #   fn handle_actions<T,U>(_:T,_:U){}
///     handle_actions(resources, actions);
/// }
/// ```
#[derive(Debug)]
pub struct Port<'a, L, A, R, C, F: Filter, S = RefCell<PtpInstanceState>> {
    config: PortConfig<()>,
    filter_config: F::Config,
    clock: C,
    // PortDS port_identity
    pub(crate) port_identity: PortIdentity,
    // Corresponds with PortDS port_state and enabled
    port_state: PortState,
    instance_state: &'a S,
    bmca: Bmca<A>,
    packet_buffer: [u8; MAX_DATA_LEN],
    lifecycle: L,
    rng: R,
    // Age of the last announce message that triggered
    // multiport disable. Once this gets larger than the
    // port announce interval, we can once again become
    // master.
    multiport_disable: Option<Duration>,

    announce_seq_ids: SequenceIdGenerator,
    sync_seq_ids: SequenceIdGenerator,
    delay_seq_ids: SequenceIdGenerator,
    pdelay_seq_ids: SequenceIdGenerator,

    filter: F,
    /// Mean delay means either `mean_path_delay` when DelayMechanism is E2E,
    /// or `mean_link_delay` when DelayMechanism is P2P.
    mean_delay: Option<Duration>,
    peer_delay_state: PeerDelayState,
    grants: unicast::Grants,
    requests: unicast::Requests,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PeerDelayState {
    Empty,
    Measuring {
        id: u16,
        responder_identity: Option<PortIdentity>,
        request_send_time: Option<Time>,
        request_recv_time: Option<Time>,
        response_send_time: Option<Time>,
        response_recv_time: Option<Time>,
    },
    PostMeasurement {
        id: u16,
        responder_identity: PortIdentity,
    },
}

/// Type state of [`Port`] entered by [`Port::end_bmca`]
#[derive(Debug)]
pub struct Running;

/// Type state of [`Port`] entered by [`Port::start_bmca`]
#[derive(Debug)]
pub struct InBmca {
    pending_action: PortActionIterator<'static>,
    local_best: Option<BestAnnounceMessage>,
}

impl<'a, A: AcceptableMasterList, C: Clock, F: Filter, R: Rng, S: PtpInstanceStateMutex>
    Port<'a, Running, A, R, C, F, S>
{
    /// Inform the port about a transmit timestamp being available
    ///
    /// `context` is the handle of the packet that was send from the
    /// [`PortAction::SendEvent`] that caused the send.
    pub fn handle_send_timestamp(
        &mut self,
        context: TimestampContext,
        timestamp: Time,
    ) -> PortActionIterator<'_> {
        match context.inner {
            actions::TimestampContextInner::Sync { id, to } => {
                self.handle_sync_timestamp(id, timestamp, to)
            }
            actions::TimestampContextInner::DelayReq { id } => {
                self.handle_delay_timestamp(id, timestamp)
            }
            actions::TimestampContextInner::PDelayReq { id } => {
                self.handle_pdelay_timestamp(id, timestamp)
            }
            actions::TimestampContextInner::PDelayResp {
                id,
                requestor_identity,
            } => self.handle_pdelay_response_timestamp(id, requestor_identity, timestamp),
        }
    }

    /// Handle the announce timer going off
    pub fn handle_announce_timer(
        &mut self,
        tlv_provider: &mut impl ForwardedTLVProvider,
    ) -> PortActionIterator<'_> {
        self.send_announce(tlv_provider)
    }

    /// Handle the sync timer going off
    pub fn handle_sync_timer(&mut self) -> PortActionIterator<'_> {
        self.send_sync()
    }

    /// Handle the unicast timer going off
    pub fn handle_unicast_timer(&mut self) -> PortActionIterator<'_> {
        self.send_unicast()
    }

    /// Handle the negotiation timer going off
    pub fn handle_negotiation_timer(&mut self) -> PortActionIterator<'_> {
        self.tick_requests()
    }

    /// Handle the delay request timer going off
    pub fn handle_delay_request_timer(&mut self) -> PortActionIterator<'_> {
        self.send_delay_request()
    }

    /// Handle the announce receipt timer going off
    pub fn handle_announce_receipt_timer(&mut self) -> PortActionIterator<'_> {
        if self
            .instance_state
            .with_ref(|state| state.default_ds.slave_only)
        {
            // We didn't hear messages from the master anymore, reset to the listening state
            // since we can't become master.
            if !matches!(self.port_state, PortState::Listening) {
                self.set_forced_port_state(PortState::Listening);
            }

            // consistent with Port<InBmca>::new()
            let duration = self.config.announce_duration(&mut self.rng);
            actions![PortAction::ResetAnnounceReceiptTimer { duration }]
        } else {
            // we didn't hear announce messages from other masters, so become master
            // ourselves
            match self.port_state {
                PortState::Master => (),
                _ => self.set_forced_port_state(PortState::Master),
            }

            // Immediately start sending syncs and announces
            actions![
                PortAction::ResetAnnounceTimer {
                    duration: core::time::Duration::from_secs(0)
                },
                PortAction::ResetSyncTimer {
                    duration: core::time::Duration::from_secs(0)
                }
            ]
        }
    }

    /// Handle the filter update timer going off
    pub fn handle_filter_update_timer(&mut self) -> PortActionIterator<'_> {
        let update = self.filter.update(&mut self.clock);
        if update.mean_delay.is_some() {
            self.mean_delay = update.mean_delay;
        }
        PortActionIterator::from_filter(update)
    }

    /// Set this [`Port`] into [`InBmca`] mode to use it with
    /// [`PtpInstance::bmca`].
    pub fn start_bmca(self) -> Port<'a, InBmca, A, R, C, F, S> {
        Port {
            port_state: self.port_state,
            instance_state: self.instance_state,
            config: self.config,
            filter_config: self.filter_config,
            clock: self.clock,
            port_identity: self.port_identity,
            bmca: self.bmca,
            rng: self.rng,
            multiport_disable: self.multiport_disable,
            packet_buffer: [0; MAX_DATA_LEN],
            lifecycle: InBmca {
                pending_action: actions![],
                local_best: None,
            },
            announce_seq_ids: self.announce_seq_ids,
            sync_seq_ids: self.sync_seq_ids,
            delay_seq_ids: self.delay_seq_ids,
            pdelay_seq_ids: self.pdelay_seq_ids,

            filter: self.filter,
            mean_delay: self.mean_delay,
            peer_delay_state: self.peer_delay_state,
            grants: self.grants,
            requests: self.requests,
        }
    }

    // parse and do basic domain filtering on message
    fn parse_and_filter<'b>(
        &mut self,
        data: &'b [u8],
    ) -> ControlFlow<PortActionIterator<'b>, Message<'b>> {
        if !is_message_buffer_compatible(data) {
            // do not spam with parse error in mixed-version PTPv1+v2 networks
            return ControlFlow::Break(actions![]);
        }
        let message = match Message::deserialize(data) {
            Ok(message) => message,
            Err(error) => {
                log::warn!("Could not parse packet: {:?}", error);
                return ControlFlow::Break(actions![]);
            }
        };
        let domain_matches = self.instance_state.with_ref(|state| {
            message.header().sdo_id == state.default_ds.sdo_id
                && message.header().domain_number == state.default_ds.domain_number
        });
        if !domain_matches {
            return ControlFlow::Break(actions![]);
        }
        ControlFlow::Continue(message)
    }

    /// Handle a message over the event channel
    pub fn handle_event_receive<'b>(
        &'b mut self,
        data: &'b [u8],
        timestamp: Time,
        from: Source,
    ) -> PortActionIterator<'b> {
        let message = match self.parse_and_filter(data) {
            ControlFlow::Continue(value) => value,
            ControlFlow::Break(value) => return value,
        };

        if self.drop_from_master(&message, from) {
            return actions![];
        }
        match message.body {
            MessageBody::Sync(sync) => self.handle_sync(message.header, sync, timestamp),
            MessageBody::DelayReq(delay_request) => {
                self.handle_delay_req(message.header, delay_request, timestamp, from)
            }
            MessageBody::PDelayReq(_) => self.handle_pdelay_req(message.header, timestamp),
            MessageBody::PDelayResp(peer_delay_response) => {
                self.handle_peer_delay_response(message.header, peer_delay_response, timestamp)
            }
            _ => self.handle_general_internal(message, from),
        }
    }

    /// Handle a general ptp message
    pub fn handle_general_receive<'b>(
        &'b mut self,
        data: &'b [u8],
        from: Source,
    ) -> PortActionIterator<'b> {
        let message = match self.parse_and_filter(data) {
            ControlFlow::Continue(value) => value,
            ControlFlow::Break(value) => return value,
        };

        if self.drop_from_master(&message, from) {
            return actions![];
        }
        self.handle_general_internal(message, from)
    }

    /// Notes where a message came from; returns whether to drop it: a
    /// multicast copy of what the master sends by unicast.
    fn drop_from_master(&mut self, message: &Message<'_>, from: Source) -> bool {
        if matches!(message.body, MessageBody::Announce(_)) {
            self.requests.heard_announce(from);
        }
        let PortState::Slave(state) = &self.port_state else {
            return false;
        };
        let sender = message.header.source_port_identity;
        if sender != state.remote_master {
            return false;
        }
        let sync = matches!(message.body, MessageBody::Sync(_));
        self.requests.heard(sender, from, sync);
        from.multicast
            && matches!(
                message.body,
                MessageBody::Sync(_) | MessageBody::FollowUp(_)
            )
            && self.requests.drops_multicast_from(sender)
    }

    fn handle_general_internal<'b>(
        &'b mut self,
        message: Message<'b>,
        from: Source,
    ) -> PortActionIterator<'b> {
        match message.body {
            MessageBody::Announce(announce) => self.handle_announce(&message, announce),
            MessageBody::FollowUp(follow_up) => self.handle_follow_up(message.header, follow_up),
            MessageBody::DelayResp(delay_response) => {
                self.handle_delay_resp(message.header, delay_response)
            }
            MessageBody::PDelayRespFollowUp(peer_delay_follow_up) => {
                self.handle_peer_delay_response_follow_up(message.header, peer_delay_follow_up)
            }
            MessageBody::Sync(_)
            | MessageBody::DelayReq(_)
            | MessageBody::PDelayReq(_)
            | MessageBody::PDelayResp(_) => {
                log::warn!("Received event message over general interface");
                actions![]
            }
            MessageBody::Signaling(_) => self.handle_signaling(&message, from),
            MessageBody::Management(_) => actions![],
        }
    }
}

impl<'a, A, C, F: Filter, R, S> Port<'a, InBmca, A, R, C, F, S> {
    /// End a BMCA cycle and make the
    /// [`handle_*`](`Port::handle_send_timestamp`) methods available again
    pub fn end_bmca(
        self,
    ) -> (
        Port<'a, Running, A, R, C, F, S>,
        PortActionIterator<'static>,
    ) {
        (
            Port {
                port_state: self.port_state,
                instance_state: self.instance_state,
                config: self.config,
                filter_config: self.filter_config,
                clock: self.clock,
                port_identity: self.port_identity,
                bmca: self.bmca,
                rng: self.rng,
                multiport_disable: self.multiport_disable,
                packet_buffer: [0; MAX_DATA_LEN],
                lifecycle: Running,
                announce_seq_ids: self.announce_seq_ids,
                sync_seq_ids: self.sync_seq_ids,
                delay_seq_ids: self.delay_seq_ids,
                pdelay_seq_ids: self.pdelay_seq_ids,
                filter: self.filter,
                mean_delay: self.mean_delay,
                peer_delay_state: self.peer_delay_state,
                grants: self.grants,
                requests: self.requests,
            },
            self.lifecycle.pending_action,
        )
    }
}

impl<L, A, R, C: Clock, F: Filter, S> Port<'_, L, A, R, C, F, S> {
    fn set_forced_port_state(&mut self, mut state: PortState) {
        log::info!(
            "new state for port {}: {} -> {}",
            self.port_identity.port_number,
            self.port_state,
            state
        );
        core::mem::swap(&mut self.port_state, &mut state);
        if matches!(self.port_state, PortState::Slave(_)) {
            self.requests.reset();
        }
        if matches!(state, PortState::Master) && !matches!(self.port_state, PortState::Master) {
            self.grants.clear();
        }
        if matches!(state, PortState::Slave(_) | PortState::Faulty)
            || matches!(self.port_state, PortState::Faulty)
        {
            let mut filter = F::new(self.filter_config.clone());
            core::mem::swap(&mut filter, &mut self.filter);
            filter.demobilize(&mut self.clock);
        }
    }
}

impl<L, A, R, C, F: Filter, S> Port<'_, L, A, R, C, F, S> {
    /// Indicate whether this [`Port`] is steering its clock.
    pub fn is_steering(&self) -> bool {
        matches!(self.port_state, PortState::Slave(_))
    }

    /// Ask the master for Sync and Delay_Resp by unicast while this port
    /// follows it (IEEE 1588-2019 16.1), or stop asking.
    pub fn set_unicast(&mut self, want: bool) {
        self.requests.set_want(want);
    }

    /// The peers to hear Announce from: by unicast, asked for, when their
    /// multicast doesn't arrive. A master heard only that way is asked for
    /// Sync and Delay_Resp by unicast as well.
    pub fn set_unicast_peers(&mut self, peers: &[PeerId]) {
        self.requests.set_known(peers);
    }

    /// Where the request for unicast Sync stands.
    pub fn unicast_state(&self) -> UnicastState {
        self.requests.state()
    }

    /// How the master this port follows reaches it, once heard.
    pub fn master_peer(&self) -> Option<PeerId> {
        match &self.port_state {
            PortState::Slave(state) => self.requests.peer_of(state.remote_master),
            _ => None,
        }
    }

    /// How many peers this port sends Sync to by unicast.
    pub fn unicast_followers(&self) -> usize {
        self.grants.followers()
    }

    /// Indicate whether this [`Port`] is in the master state.
    pub fn is_master(&self) -> bool {
        matches!(self.port_state, PortState::Master)
    }

    pub(crate) fn state(&self) -> &PortState {
        &self.port_state
    }

    pub(crate) fn number(&self) -> u16 {
        self.port_identity.port_number
    }

    /// Get a copy of the port dataset of the port
    pub fn port_ds(&self) -> PortDS {
        PortDS {
            port_identity: self.port_identity,
            port_state: match self.port_state {
                PortState::Faulty => observability::port::PortState::Faulty,
                PortState::Listening => observability::port::PortState::Listening,
                PortState::Master => observability::port::PortState::Master,
                PortState::Passive => observability::port::PortState::Passive,
                PortState::Slave(_) => observability::port::PortState::Slave,
            },
            log_announce_interval: self.config.announce_interval.as_log_2(),
            announce_receipt_timeout: self.config.announce_receipt_timeout,
            log_sync_interval: self.config.sync_interval.as_log_2(),
            delay_mechanism: match self.config.delay_mechanism {
                crate::config::DelayMechanism::E2E { interval } => {
                    observability::port::DelayMechanism::E2E {
                        log_min_delay_req_interval: interval.as_log_2(),
                    }
                }
                crate::config::DelayMechanism::P2P { interval } => {
                    observability::port::DelayMechanism::P2P {
                        log_min_p_delay_req_interval: interval.as_log_2(),
                        mean_link_delay: self.mean_delay.map(|v| v.into()).unwrap_or_default(),
                    }
                }
            },
            version_number: 2,
            minor_version_number: self.config.minor_ptp_version as u8,
            delay_asymmetry: self.config.delay_asymmetry.into(),
            master_only: self.config.master_only,
        }
    }

    /// If this port is in the slave state, this returns the current estimate
    /// of the current_ds offset_to_master and mean_delay fields.
    pub fn port_current_ds_contribution(&self) -> Option<FilterEstimate> {
        if matches!(self.port_state, PortState::Slave(_)) {
            Some(self.filter.current_estimates())
        } else {
            None
        }
    }
}

impl<'a, A, C, F: Filter, R: Rng, S: PtpInstanceStateMutex> Port<'a, InBmca, A, R, C, F, S> {
    /// Create a new port from a port dataset on a given interface.
    pub(crate) fn new(
        instance_state: &'a S,
        config: PortConfig<A>,
        filter_config: F::Config,
        clock: C,
        port_identity: PortIdentity,
        mut rng: R,
    ) -> Self {
        let duration = config.announce_duration(&mut rng);
        let bmca = Bmca::new(
            config.acceptable_master_list,
            config.announce_interval.as_duration().into(),
            port_identity,
        );

        let filter = F::new(filter_config.clone());

        Port {
            config: PortConfig {
                acceptable_master_list: (),
                delay_mechanism: config.delay_mechanism,
                announce_interval: config.announce_interval,
                announce_receipt_timeout: config.announce_receipt_timeout,
                sync_interval: config.sync_interval,
                master_only: config.master_only,
                delay_asymmetry: config.delay_asymmetry,
                minor_ptp_version: config.minor_ptp_version,
            },
            filter_config,
            clock,
            port_identity,
            port_state: PortState::Listening,
            instance_state,
            bmca,
            rng,
            multiport_disable: None,
            packet_buffer: [0; MAX_DATA_LEN],
            lifecycle: InBmca {
                pending_action: actions![
                    PortAction::ResetAnnounceReceiptTimer { duration },
                    PortAction::ResetNegotiationTimer {
                        duration: core::time::Duration::ZERO
                    }
                ],
                local_best: None,
            },
            announce_seq_ids: SequenceIdGenerator::new(),
            sync_seq_ids: SequenceIdGenerator::new(),
            delay_seq_ids: SequenceIdGenerator::new(),
            pdelay_seq_ids: SequenceIdGenerator::new(),
            filter,
            mean_delay: None,
            peer_delay_state: PeerDelayState::Empty,
            grants: Default::default(),
            requests: Default::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use core::cell::RefCell;

    use super::*;
    use crate::{
        config::{
            AcceptAnyMaster, DelayMechanism, InstanceConfig, PtpMinorVersion, TimePropertiesDS,
        },
        datastructures::datasets::{InternalDefaultDS, InternalParentDS, PathTraceDS},
        filters::BasicFilter,
        time::{Duration, Interval, Time},
        Clock,
    };

    // General test infra
    pub(super) struct TestClock;

    impl Clock for TestClock {
        type Error = ();

        fn set_frequency(&mut self, _freq: f64) -> Result<Time, Self::Error> {
            Ok(Time::default())
        }

        fn now(&self) -> Time {
            panic!("Shouldn't be called");
        }

        fn set_properties(
            &mut self,
            _time_properties_ds: &TimePropertiesDS,
        ) -> Result<(), Self::Error> {
            Ok(())
        }

        fn step_clock(&mut self, _offset: Duration) -> Result<Time, Self::Error> {
            Ok(Time::default())
        }
    }

    pub(super) fn setup_test_port(
        state: &RefCell<PtpInstanceState>,
    ) -> Port<'_, Running, AcceptAnyMaster, rand::rngs::mock::StepRng, TestClock, BasicFilter> {
        let port = Port::<_, _, _, _, BasicFilter>::new(
            state,
            PortConfig {
                acceptable_master_list: AcceptAnyMaster,
                delay_mechanism: DelayMechanism::E2E {
                    interval: Interval::from_log_2(1),
                },
                announce_interval: Interval::from_log_2(1),
                announce_receipt_timeout: 3,
                sync_interval: Interval::from_log_2(0),
                master_only: false,
                delay_asymmetry: Duration::ZERO,
                minor_ptp_version: PtpMinorVersion::One,
            },
            0.25,
            TestClock,
            Default::default(),
            rand::rngs::mock::StepRng::new(2, 1),
        );

        let (port, _) = port.end_bmca();
        port
    }

    pub(super) fn setup_test_port_custom_identity(
        state: &RefCell<PtpInstanceState>,
        port_identity: PortIdentity,
    ) -> Port<'_, Running, AcceptAnyMaster, rand::rngs::mock::StepRng, TestClock, BasicFilter> {
        let port = Port::<_, _, _, _, BasicFilter>::new(
            state,
            PortConfig {
                acceptable_master_list: AcceptAnyMaster,
                delay_mechanism: DelayMechanism::E2E {
                    interval: Interval::from_log_2(1),
                },
                announce_interval: Interval::from_log_2(1),
                announce_receipt_timeout: 3,
                sync_interval: Interval::from_log_2(0),
                master_only: false,
                delay_asymmetry: Duration::ZERO,
                minor_ptp_version: PtpMinorVersion::One,
            },
            0.25,
            TestClock,
            port_identity,
            rand::rngs::mock::StepRng::new(2, 1),
        );

        let (port, _) = port.end_bmca();
        port
    }

    pub(super) fn setup_test_port_custom_filter<F: Filter>(
        state: &RefCell<PtpInstanceState>,
        filter_config: F::Config,
    ) -> Port<'_, Running, AcceptAnyMaster, rand::rngs::mock::StepRng, TestClock, F> {
        let port = Port::<_, _, _, _, F>::new(
            state,
            PortConfig {
                acceptable_master_list: AcceptAnyMaster,
                delay_mechanism: DelayMechanism::E2E {
                    interval: Interval::from_log_2(1),
                },
                announce_interval: Interval::from_log_2(1),
                announce_receipt_timeout: 3,
                sync_interval: Interval::from_log_2(0),
                master_only: false,
                delay_asymmetry: Duration::ZERO,
                minor_ptp_version: PtpMinorVersion::One,
            },
            filter_config,
            TestClock,
            Default::default(),
            rand::rngs::mock::StepRng::new(2, 1),
        );

        let (port, _) = port.end_bmca();
        port
    }

    pub(super) fn setup_test_state() -> RefCell<PtpInstanceState> {
        let default_ds = InternalDefaultDS::new(InstanceConfig {
            clock_identity: Default::default(),
            priority_1: 255,
            priority_2: 255,
            domain_number: 0,
            slave_only: false,
            sdo_id: Default::default(),
            path_trace: false,
        });

        let parent_ds = InternalParentDS::new(default_ds);

        RefCell::new(PtpInstanceState {
            default_ds,
            current_ds: Default::default(),
            parent_ds,
            time_properties_ds: Default::default(),
            path_trace_ds: PathTraceDS::new(false),
        })
    }
}
