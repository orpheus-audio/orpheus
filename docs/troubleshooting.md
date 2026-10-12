# Troubleshooting

Start with `orpheusctl status` on both nodes and the `orpheusd` log.

## Logs

`orpheusd` logs to stderr at the levels error, warn, info and debug; by
default it shows info and above. `RUST_LOG` changes that, also per module:

```sh
RUST_LOG=debug orpheusd
RUST_LOG=info,orpheus_clock=debug orpheusd
```

As a systemd service its log goes to the journal, with each line's level as
the journal priority:

```sh
journalctl --user -u orpheusd -f          # follow
journalctl --user -u orpheusd -p warning  # warnings and errors only
```

Fallbacks are logged as warnings with the reason, for example
`PTP unavailable; using the system clock error=…`.

## `clock: system` instead of PTP

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
  5004, and the API TCP 7480. With firewalld:

  ```sh
  sudo firewall-cmd --permanent --add-port=319-320/udp --add-port=5004/udp --add-port=7480/tcp
  sudo firewall-cmd --reload
  ```

  With ufw: `sudo ufw allow 319:320/udp`, `sudo ufw allow 5004/udp`,
  `sudo ufw allow 7480/tcp`.
- **Every node is master.** If each node reports `master` and itself as
  grandmaster, they don't hear each other's announce messages. Check the
  firewall, the `domain`, and that all nodes use the same transport (the
  status shows `/ipv4` or `/ipv6`).
- **Wrong interface.** On a machine with Ethernet and Wi-Fi, the default
  route may be on the wrong one. Set `[ptp] interface`.
- **It's still settling.** A cold start takes 30 to 60 s, sometimes with one
  overshoot. Wait a minute before drawing conclusions.
- **Wi-Fi.** PTP over Wi-Fi is unreliable. Access points send multicast at a
  low rate, and with power saving they hold it back for 100 to 300 ms. Use
  Ethernet; a Wi-Fi profile is planned.

## PTP works for a few minutes, then stops

This is the classic sign of **IGMP or MLD snooping without a querier**. A
managed switch, or a router or access point with snooping enabled, forwards
multicast only to ports that joined the group. Joins expire after a few
minutes unless a querier asks the hosts to renew them, so PTP traffic stops
reaching the nodes.

Fix it on the switch or router: enable an IGMP querier (MLD querier for
IPv6), or disable snooping. Unmanaged switches don't snoop and are not
affected.

## No sound

1. On the receiving node, `orpheusctl status` should show `ssrc …` and a
   rising packet count. If it stays `idle`:
   - the sender points to the wrong address or port (`sender … → address`
     on the sending node);
   - a firewall blocks UDP 5004;
   - the receiver is disabled, or `[receiver] port` differs.
2. If packets arrive but count as `late` or `early`, the clocks disagree. See
   the next section.
3. Check that the sender's sink actually gets audio: the application must play
   into the sink named after the peer, for example chosen in `pavucontrol` or with
   `wpctl set-default`.
4. Check where the receiver plays: the default output unless
   `[receiver] target` names another one.

## Dropouts and clicks

- **`late` rises, `margin` near zero:** packets arrive too close to their
  playout time. Raise `link_offset_ms`, or make sure both nodes are settled
  on PTP. On the system clock, NTP decides how well the nodes agree.
- **`lost` rises:** the network drops packets. Wi-Fi in the path, an
  overloaded link, or a switch that floods multicast.
- **`underruns` rise:** the audio system or a network thread doesn't keep up.
  This happens under heavy CPU load, because Orpheus's threads don't run with
  realtime priority yet. A larger `[audio] quantum` helps.
- **`realigns` rise:** a thread stalled for more than 100 ms, or the clock
  jumped. Look for steps in the PTP status, or NTP stepping the system clock.

## The system clock jumps

The PTP clock sits on top of the system clock. If NTP **steps** the system
clock (as at boot or after a long suspend), the PTP clock jumps with it until
the filter corrects it. Gradual NTP adjustments (slewing) are harmless.

## AES67 and Dante devices don't see the node

- They speak PTP over IPv4 only. Use an interface with an IPv4 address, or
  set `transport = "ipv4"`.
- Discovery (SAP, mDNS) and SDP are not implemented yet, so the devices
  cannot find the node's streams. Interoperability is untested; see the
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
