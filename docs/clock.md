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
interface, using [statime](https://github.com/pendulum-project/statime).

**Grandmaster election.** The best master clock algorithm elects one
grandmaster per domain. A dedicated AES67 or Dante grandmaster wins if there
is one, because nodes default to `priority1 = 250`. Otherwise one of the
nodes becomes grandmaster: the one with the lowest `priority1`, then
`priority2`, then clock identity. Set `slave_only = true` on nodes that should
never lead.

**Virtual clock.** A node does not change the system clock, which stays with
NTP. It keeps a virtual clock on top of the system clock instead.
statime's Kalman filter estimates the offset to the grandmaster and adjusts
the virtual clock:

- Offsets above `step_threshold_us` (1 ms) are stepped in one go. This
  happens on a cold start.
- Smaller offsets are slewed: the virtual clock runs slightly fast or slow
  until it catches up. `orpheusctl status` shows this rate as `ppm` (parts
  per million; 1 ppm is 1 µs per second, or 3.6 ms per hour).

**Servo.** Two changes to statime's defaults make a cold start fast:

- After a step, measurements are dropped for 0.5 s and the filter starts
  afresh. statime keeps its estimate of the measurement noise across a step,
  and a pair of measurements from both sides of the step, off by the whole
  step, made it distrust everything for about 30 s and then overshoot.
- The filter estimates the measurement noise after two measurement pairs
  instead of four, and corrects offsets within about 1 s instead of 2.

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

**Privileges.** PTP uses UDP ports 319 (event messages) and 320 (general
messages). Binding ports below 1024 needs `CAP_NET_BIND_SERVICE`:

```sh
sudo setcap cap_net_bind_service=+ep target/release/orpheusd
```

The capability is lost whenever the binary is rebuilt or replaced.

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

## Fallback: the system clock

The node uses the system clock (on the TAI timescale) when:

- `[ptp] enabled = false`,
- the PTP ports cannot be bound (missing `CAP_NET_BIND_SERVICE`),
- there is no default route and no `interface` is configured,
- the interface has no MAC address (VPN and other tunnel interfaces) or,
  with `transport = "auto"`, no IP address.

`orpheusctl status` then shows `clock: system`, and the log says why. Nodes
on the system clock stay in sync only as well as NTP keeps their clocks
together, which on a LAN is typically within a few milliseconds. That must
fit into the link offset. Raise `link_offset_ms` if receivers report late
packets.

## Clock-domain crossing (ASRC)

The sound cards don't run on the media clock. Each has its own crystal, which
deviates by up to a few hundred ppm. Without correction a receiver would drop
or repeat audio every few minutes.

So every sender and receiver resamples between the media clock and the sound
card's clock. A control loop watches the fill level of the buffer between the
network thread and PipeWire, and nudges the resampling ratio so that the
level, and with it the latency, stays where it was when the stream started.
The correction is limited to ±1000 ppm. `orpheusctl status` shows it as
`ASRC ±x ppm`.

The loop starts with a period of 10 s to acquire the drift quickly and slows
down to 60 s. While it acquires, the latency may wander by about a
millisecond.
