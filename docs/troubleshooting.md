# Troubleshooting

Start with `orpheusctl status` on both nodes and the `orpheusd` log.

## Logs

`orpheusd` logs to stderr at the levels error, warn, info and debug; by
default it shows info and above, from statime (the PTP stack) only
warnings, because it logs every measurement. `RUST_LOG` changes that, also
per module:

```sh
RUST_LOG=debug orpheusd
RUST_LOG=info,orpheus_clock=debug orpheusd
RUST_LOG=info,statime=info orpheusd   # every PTP measurement
```

As a systemd service its log goes to the journal, with each line's level as
the journal priority:

```sh
journalctl --user -u orpheusd -f          # follow
journalctl --user -u orpheusd -p warning  # warnings and errors only
```

Fallbacks are logged as warnings with the reason, for example
`PTP unavailable; using the system clock error=…`.

## `system clock` instead of PTP

The log line `PTP unavailable; using the system clock` gives the reason.

- **Permission denied:** the binary lacks `CAP_NET_BIND_SERVICE` for ports
  319 and 320. Run `sudo setcap cap_net_bind_service=+ep path/to/orpheusd`
  again; every rebuild removes it. `getcap path/to/orpheusd` shows whether it
  is set.
- **Interface has no MAC address:** the default route goes through a VPN or
  another tunnel (WireGuard, OpenVPN tun). Set `[ptp] interface` to the LAN
  interface.
- **No default route:** set `[ptp] interface`.

## PTP runs but never settles

- **Firewall.** PTP needs UDP 319 and 320 in both directions; RTP needs UDP
  5004 to 5018 (one port in two per receiver, up to `[receiver] max`; see
  [configuration](configuration.md#receiver)), mDNS UDP 5353, and the API
  TCP 7480. With firewalld:

  ```sh
  sudo firewall-cmd --permanent --add-port=319-320/udp --add-port=5004-5018/udp --add-port=7480/tcp --add-service=mdns
  sudo firewall-cmd --reload
  ```

  With ufw: `sudo ufw allow 319:320/udp`, `sudo ufw allow 5004:5018/udp`,
  `sudo ufw allow 5353/udp`, `sudo ufw allow 7480/tcp`.
- **Every node is master.** If each node reports `master` and itself as
  grandmaster, they don't hear each other's announce messages. Check the
  firewall, the `domain`, and that all nodes use the same transport (the
  status shows `/ipv4` or `/ipv6`).
- **Wrong interface.** On a machine with Ethernet and Wi-Fi, the default
  route may be on the wrong one. Set `[ptp] interface`.
- **It's still settling.** A node settles about 10 s after it hears the
  grandmaster; if all nodes start at once, the election adds about 10 s.
- **Wi-Fi.** PTP over Wi-Fi is unreliable. Access points send multicast at a
  low rate, and with power saving they hold it back for 100 to 300 ms. Use
  Ethernet; a Wi-Fi profile is planned. The node warns about it (`wifi`,
  `ptp_delay` or `ptp_steps` in the [warnings](usage.md#warnings)).
- **Unsteady path.** If `steps` keeps rising (warning `ptp_steps`), the
  delay to the grandmaster jumps by more than `step_threshold_us`, as on
  Wi-Fi. Set `[ptp] step_threshold_us = 20000`; see
  [clock](clock.md#ptp).

## PTP works for a few minutes, then stops

This is the classic sign of **IGMP or MLD snooping without a querier**. A
managed switch, or a router or access point with snooping enabled, forwards
multicast only to ports that joined the group. Joins expire after a few
minutes unless a querier asks the hosts to renew them, so PTP traffic stops
reaching the nodes.

Fix it on the switch or router: enable an IGMP querier (MLD querier for
IPv6), or disable snooping. Unmanaged switches don't snoop and are not
affected.

## `connect` fails

- `409 … is playing another sender`: all of the peer's receivers are
  connected (`[receiver] max` on the peer). Disconnect one of them, raise
  `max` there, or take one over with `--force`. A receiver whose sender
  vanished is freed within 30 seconds once the sending node is back; if it
  stays away, use `--force`.
- `409 already connected to … as sender N`: this node already sends there
  with the same priority. If `status` shows that sender as `disconnected,
  paused`, it takes the next free receiver of the peer by itself.
- `409 other senders took …'s free receivers meanwhile`: other nodes
  connected at the same moment; try again.
- `502 … no answer`: the peer's API is unreachable: wrong name or address,
  the node is down, or a firewall blocks TCP 7480. `orpheusctl nodes` shows
  whether the node was discovered and under which address.
- `502 … refused`: the peer's receiver rejected the connection; the message
  says why.
- `400 … the peer's receiver chooses the port`: a port goes only with
  `--send-only`. To reach a node whose API is on another port, give its URL,
  for example `http://192.168.1.20:7481`.

## No sound

1. On the receiving node, `orpheusctl status` should show `receiving from …`
   and a rising packet count.
   - only `free, port …`: no sender is connected, and by default a
     receiver plays only a connected sender. Connect with `orpheusctl
     connect` (not `--send-only`), or set `[receiver] accept = "any"`.
   - `connected to …, idle`: the sender points to the wrong address or port
     (the `PEER` column on the sending node), or a firewall blocks the
     receiver's port (5004 to 5018);
   - `receiving from …, muted`: another stream plays and the
     [policy](usage.md#several-streams) gives it precedence. With the
     default `last`, the stream is heard again once the other one has been
     quiet for 5 seconds; set `[receiver] policy = "mix"` to hear all.
   - no receiver at all: they are disabled in `[receiver]`.

   On the sending node, a sender shown as `(disconnected, paused)` or
   `(off)` sends nothing; see [reading the
   status](usage.md#reading-the-status).
2. If packets arrive but count as `late` or `early`, the clocks disagree. See
   the next section.
3. Check that the sender's sink actually gets audio: the application must play
   into the sink named after the peer, for example chosen in `pavucontrol` or with
   `wpctl set-default`.
4. Check where the receiver plays: the default output unless its stream was
   moved in `pavucontrol` or with `wpctl`, and its volume there.
   WirePlumber keeps both per sending node.
5. Check the receiver's connection: `orpheusctl status` shows `off` if it was
   switched off, and `ignored` packets if it only takes another sender. A
   stream sent to it with `--send-only` is ignored while another sender is
   connected, or with `accept = "connected"`:

   ```sh
   curl -s localhost:7480/x-nmos/connection/v1.1/single/receivers/ | jq
   curl -s localhost:7480/x-nmos/connection/v1.1/single/receivers/<id>/active | jq
   ```

   Packets to another port than the receiver listens on (`listen`) don't
   arrive at all.

## Dropouts and clicks

- **Warnings** in `orpheusctl status` name Wi-Fi or a jittery path on
  either end; see [warnings](usage.md#warnings).
- **`late` rises, `margin` near zero:** packets arrive too close to their
  playout time. Raise `link_offset_ms`, or make sure both nodes are settled
  on PTP. On the system clock, NTP decides how well the nodes agree.
- **`lost` rises:** the network drops packets. Wi-Fi in the path, an
  overloaded link, or a switch that floods multicast.
- **`underruns` rise:** the audio system or a network thread doesn't keep up.
  This happens under heavy CPU load, because Orpheus's threads don't run with
  realtime priority yet. A larger block (`[audio] block_frames` or
  `cycle_ms`) helps.
- **`realigns` rise:** a thread stalled for more than 100 ms, or the clock
  jumped. Look for steps in the PTP status, or NTP stepping the system clock.

## The system clock jumps

The PTP clock sits on top of the system clock. If NTP **steps** the system
clock (as at boot or after a long suspend), the PTP clock jumps with it until
the filter corrects it. Gradual NTP adjustments (slewing) are harmless.

## `orpheusctl nodes` doesn't list a node

- **Firewall.** mDNS needs UDP 5353; see [above](#ptp-runs-but-never-settles).
- **Different networks.** mDNS does not cross routers, and many access
  points filter it between Wi-Fi and Ethernet or between guest networks.
- **Just started.** A node announces itself about one second after it
  starts. A node that crashed instead of stopping stays listed as online for
  up to two minutes.
- `avahi-browse -rt _nmos-node._tcp` shows what the machine hears.

## AES67 and Dante devices don't see the node

- They speak PTP over IPv4 only. Use an interface with an IPv4 address, or
  set `transport = "ipv4"`.
- SAP is not implemented, so the devices
  cannot find the node's streams by themselves. Each sender has an SDP file
  at `/x-orpheus/v1/senders/{id}/sdp`, which some devices can import. Interoperability is untested; see the
  [roadmap](roadmap.md).

## Two nodes on one machine

Both default to the same directories, UDP 5004 and TCP 7480. Their PTP instances
don't hear each other either, because multicast loopback is off. See
[development](development.md#two-nodes-on-one-machine).

## Another PTP daemon on the same machine

ptp4l, statime or pipewire-aes67 with ptp4l can share ports 319 and 320 with
Orpheus, so there is no error. Both take part in PTP as separate clocks, and
one of them may become the grandmaster. That works, but on a node it is
simpler to run only Orpheus.
