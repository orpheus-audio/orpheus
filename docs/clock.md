# Clock

Nodes play in sync because they share a clock. A sender stamps every packet
with the time its first sample was captured, and every receiver plays it at
that time plus the link offset. The shared clock comes from PTP; without PTP
a node falls back to its system clock.

## The media clock

The clock counts nanoseconds of TAI since the PTP epoch, 1970-01-01 00:00:00
TAI. TAI is atomic time without leap seconds; it runs 37 s ahead of UTC.
AES67 and PTP use it because it never jumps.

An RTP timestamp is the number of samples (frames) since that epoch at the
sampling instant, truncated to 32 bits (`a=mediaclk:direct=0` in AES67
terms). At 48 kHz the 32-bit counter wraps about every 25 hours; a receiver
extends it using its own clock.

The node takes TAI − UTC from the kernel if something set it (ptp4l or
chrony's `leapsectz` do), otherwise it assumes 37 s.

## PTP

Each node runs a PTP (IEEE 1588-2019) instance with one port on its LAN
interface, using `orpheus-ptp`, a copy of
[statime](https://github.com/pendulum-project/statime) 0.4.0.

**Grandmaster election.** The best master clock algorithm elects one
grandmaster per domain. A dedicated AES67 or Dante grandmaster wins if there
is one, because nodes default to `priority1 = 250`. Otherwise one of the
nodes becomes grandmaster: the one with the lowest `priority1`, then
`priority2`, then clock identity. Set `slave_only = true` on nodes that should
never lead.

**Wired nodes first.** Every follower measures its path to the grandmaster,
so a grandmaster on Wi-Fi puts the access point into everyone's path. With
`priority2 = "auto"`, the default, a node announces `priority2` 127 while
PTP runs on a wired interface and 129 while it runs on Wi-Fi. A wired node
then becomes grandmaster before one on Wi-Fi, and takes over from it when
it joins. The status shows the value in use (`clock.ptp.priority2`). A
number for `priority2` turns this off for that node, and `priority1`
decides before either.

A node whose PTP comes back on a wired interface, after its cable, dock or
switch was gone or after it was on Wi-Fi, counts as on Wi-Fi for one more
minute. A loose contact then doesn't change the grandmaster each time the
node returns: it follows the grandmaster that took over, and takes the
role back only once it has stayed for a minute. If it becomes grandmaster
all the same, because no wired node is left, it counts as wired after
15 s. So when a switch restarts and all its nodes return together, they
elect among themselves as usual, and none takes over a minute later.

**Virtual clock.** A node does not change the system clock, which stays with
NTP. It keeps a virtual clock on top of the system clock instead.
statime's Kalman filter estimates the offset to the grandmaster and adjusts
the virtual clock:

- Offsets above `step_threshold_us` (2 ms) are stepped in one go. This
  happens on a cold start.
- Smaller offsets are slewed: the virtual clock runs slightly fast or slow
  until it catches up. `orpheusctl status` shows this rate as `frequency`
  in ppm (parts per million; 1 ppm is 1 µs per second, or 3.6 ms per hour).

**Servo.** Five changes to statime's defaults make a cold start fast and
let the clock settle behind a slow or unsteady path:

- After a step, measurements are dropped for 0.5 s and the filter starts
  afresh. statime keeps its estimate of the measurement noise across a step,
  and a pair of measurements from both sides of the step, off by the whole
  step, made it distrust everything for about 30 s and then overshoot.
- The filter estimates the measurement noise after two measurement pairs
  instead of four, and corrects offsets within about 1 s instead of 2.
- The servo estimates the path delay itself, as the median of the last 15
  pairs of a sync and a delay request (within 250 ms of each other, so the
  offset cancels), and hands the filter only what is left of it. statime
  compares each raw measurement, offset plus or minus the delay, with its
  offset alone and starts over when they differ by more than
  `step_threshold_us`, so with more than half a millisecond of delay each
  way it stepped over and over and never settled. The servo waits for the
  first pair before it steers, about a second at most, and keeps the
  estimate across steps, which don't change the path.
- A gate keeps out measurements that waited on the way. Waiting only ever
  adds time, so the fastest measurements are the truthful ones. For each
  direction the gate compares a measurement with what the filter predicts
  and passes it only if it arrived at most 0.5 ms later than the fastest
  of the last 8 s of syncs, or 16 s of delay requests. On Wi-Fi the access
  point holds multicast until the next beacon, up to about 100 ms, while
  any client dozes; without the gate a single held message made the filter
  start over and step, and the next one step back. A wired path stays
  within the margin, so there the gate passes everything. A real change of
  offset or path passes once the faster measurements from before it have
  left the window, after 8 to 16 s. The gate waits until the filter has
  taken 8 measurements after a start or a step.
- A measurement further from the filter's estimate than
  `step_threshold_us` is held back until 8 in a row are. statime starts
  over and steps on a single one, and right after a step, while the gate
  still passes everything, on Wi-Fi that was usually the next one, and
  the next: a node stepped 40 to 120 times before it settled. A real
  change of offset persists and is stepped 1 s later.

With a path delay that changes suddenly by more than the step threshold
the clock steps a few times until the median follows (about 8 s), after
the gate let the change through. In a simulation of Wi-Fi (a 3 ms path,
300 µs of jitter, and bursts of 0.5 to 5 s every 5 to 20 s in which each
message waits 0 to 102 ms), the clock stayed within 50 µs of the
grandmaster for 10 minutes without a step; without the gate it stepped
194 times and was up to 100 ms off.

Measured on Wi-Fi at home (two nodes on one access point, one of them
with a weak signal of about −75 dBm), messages waited about 1 ms, often
several, and up to 120 ms every 5 minutes while that node scanned for
other access points. With the defaults a node settled 30 to 40 s after it
started, stepping once or twice, and then stayed within 13 µs of the
grandmaster for 40 minutes without a step. With a 1 ms threshold and
without holding back single measurements it stepped 40 to 120 times
first. Replaying the 56 minutes of recorded waits from 11 starting
points, the clock stepped twice in all and was at worst 3 ms off.

Measured between two nodes on one machine (network namespaces, 50 µs of
added jitter, clock skews up to ±200 ppm), a node that joins a running
network follows the grandmaster to within 100 µs about 3 s after it hears
it (at most 8 s), and stays within a few µs. Real networks and crystals will
be somewhat worse; see the [roadmap](roadmap.md).

**Settling.** The clock counts as settled once its offset has stayed below
`settle_threshold_us` (100 µs) for three announce intervals in a row (6 s).
This takes about 10 s after a node joins a running network. The grandmaster
always counts as settled. If all nodes start at once, electing the
grandmaster adds about 10 s.

**Timestamps.** Packet times come from the kernel's software timestamps.
Their error is roughly 15 to 30 µs, well within the default link offset of
10 ms. Hardware timestamping is planned; see the [roadmap](roadmap.md).

Some interfaces, many Wi-Fi drivers among them, give no send timestamps
(`ethtool -T <interface>` lists no `software-transmit`). After three missing
in a row the node logs a warning and stamps its PTP messages just before
sending instead. That misses the time a packet spends in the kernel and
driver, some tens of µs, which shows up as a larger `delay` and, if only one
end stamps this way, as an offset error of about half of it. On Wi-Fi the
network jitter is far larger anyway.

**Privileges.** PTP uses UDP ports 319 (event messages) and 320 (general
messages). Binding ports below 1024 needs `CAP_NET_BIND_SERVICE`. On
Wi-Fi the node also turns power saving off, which needs `CAP_NET_ADMIN`
(see [troubleshooting](troubleshooting.md#wi-fi-power-saving)):

```sh
sudo setcap cap_net_bind_service,cap_net_admin=ep target/release/orpheusd
```

`CAP_NET_ADMIN` lets a program change any network setting, firewall
included; the node uses it for nothing else. Leave it out on a wired
machine. The capabilities are lost whenever the binary is rebuilt or
replaced.

## IPv4 and IPv6

PTP runs over UDP on one transport per node:

| Transport | Multicast groups |
|---|---|
| IPv4 | 224.0.1.129 (all messages), 224.0.0.107 (peer delay) |
| IPv6 | ff0e::181 (all messages), ff02::6b (peer delay) |

With `transport = "auto"` a node uses IPv4 if its PTP interface has an IPv4
address, and IPv6 otherwise. So in a mixed network all nodes pick IPv4, and in
an IPv6-only network all of them pick IPv6. Nodes on different transports do
not see each other, and AES67 and Dante devices only speak IPv4.

The node measures the path delay end to end (delay request/response), as
AES67 requires.

RTP and the HTTP API listen on IPv4 and IPv6 regardless of the PTP
transport.

## Unicast

PTP goes by multicast, as AES67 and Dante expect. Over Wi-Fi that hurts:
an access point sends multicast at its lowest rate, without acknowledging
or repeating it, and holds it back until the next beacon while any client
dozes. Unicast it sends at once and repeats when lost. Wi-Fi anywhere on
the path matters, not only at the follower: a master on Wi-Fi gets every
follower's delay requests as multicast from the access point.

So a follower asks its master for Sync and Delay_Resp by unicast, as
IEEE 1588-2019 (16.1, unicast negotiation) defines, when (`[ptp] unicast
= "auto"`):

- its PTP interface is on Wi-Fi, or
- its master is an Orpheus node that announces Wi-Fi, or
- the measurements spread widely: more than 250 µs (the 90th percentile
  of how late they come, against the earliest, shown as `path jitter`).
  That catches Wi-Fi the node can't see, such as a repeater or a mesh
  link between two wired nodes. Once asked, it stays with unicast for as
  long as it follows that grandmaster.

The master grants it for 60 s at a time, and the follower renews halfway.
While it has the grant, the follower takes Sync and Follow_Up from its
master only by unicast, drops their multicast copies and sends its delay
requests by unicast; Announce stays multicast, so the election still sees
every clock. The master keeps sending multicast to everyone else, and
sends each follower with a grant its own Sync, with its own send time.
If unicast Sync stops for a few seconds, say because the master
restarted, the follower takes multicast again and asks anew.

A master that refuses, or doesn't answer within 5 s, such as a PTP device
without unicast negotiation, leaves the follower on multicast; it asks
again after a minute and warns (`ptp_multicast`) if Wi-Fi is on the path.
`"always"` asks every master, `"never"` none.

**Multicast that doesn't arrive.** Some networks drop PTP multicast on the
way to a node, typically because its firewall drops IGMP and the switch or
access point stops forwarding the group to it (see
[troubleshooting](troubleshooting.md#ptp-works-for-a-few-minutes-then-stops)). mDNS
still arrives, so the node knows the other Orpheus nodes. If it hears no
multicast Announce from one of them for 10 s, it asks that node for
Announce by unicast; only a master grants it, the followers send none. The
election then sees that clock, and a master heard only by unicast is asked
for Sync and Delay_Resp by unicast as well. PTP devices that aren't
Orpheus nodes can be listed in `[ptp] unicast_masters`.

## When PTP can't run

The PTP clock runs on top of the system clock (on the TAI timescale): it
is the system clock plus an offset and a rate correction, which PTP
adjusts. So the node always has a clock, also while PTP is not running.

**At start**, PTP may not be able to run yet:

- the PTP ports cannot be bound (missing `CAP_NET_BIND_SERVICE`),
- there is no default route and no `interface` is configured,
- the interface has no MAC address (VPN and other tunnel interfaces) or,
  with `transport = "auto"`, no IP address.

The node then runs on the system clock and tries again every 5 seconds,
and as soon as an interface comes up or changes. Once PTP runs, the clock
follows the grandmaster; as with any start, that may take a step.

**While it runs**, PTP stops when its interface goes down, the default
route moves to another interface, or the interface gets a new IPv4
address. The clock then **holds over**: it keeps the grandmaster's last
time and rate and drifts away from the other nodes only as fast as that
rate was off or the crystal changes since, typically by less than a few µs
per second. PTP starts again by
itself, on the new interface if the route moved, and the clock continues
from where it is, without a step if it is still within `step_threshold_us`
of the grandmaster.

Changing the PTP [settings](configuration.md#settings-changed-over-the-api)
over the API restarts PTP the same way, without a jump.

`orpheusctl status` shows `PTP not running, system clock` or `PTP stopped,
holding over` with the reason, and the node [warns](usage.md#warnings)
(`ptp_down`). Nodes on the system clock stay in sync only as well as NTP
keeps their clocks together, which on a LAN is typically within a few
milliseconds. That must fit into the link offset. Raise `link_offset_ms` if
receivers report late packets.

With `[ptp] enabled = false` the node uses the system clock only.

## Clock-domain crossing (ASRC)

The sound cards don't run on the media clock. Each has its own crystal, which
deviates by up to a few hundred ppm. Without correction a receiver would drop
or repeat audio every few minutes.

So every sender and receiver resamples between the media clock and the sound
card's clock. A control loop watches the fill level of the buffer between the
network thread and PipeWire, and nudges the resampling ratio so that the
level, and with it the latency, stays where it was when the stream started.
The correction is limited to ±1000 ppm. `orpheusctl status` shows it as
`ASRC`.

The loop starts with a period of 10 s to acquire the drift quickly and slows
down to 60 s. While it acquires, the latency may wander by about a
millisecond.
