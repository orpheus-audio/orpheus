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

## `PTP not running` or `holding over`

`orpheusctl status` shows the reason as `error`, and the log says it once
with `PTP can't start; trying again` or `PTP stopped`. The node keeps
trying by itself (see [clock](clock.md#when-ptp-cant-run)); fix the cause
and it follows the grandmaster again within seconds, without a restart.

- **Permission denied:** the binary lacks `CAP_NET_BIND_SERVICE` for ports
  319 and 320. Run `sudo setcap cap_net_bind_service,cap_net_admin=ep
  path/to/orpheusd` again; every rebuild removes it. `getcap path/to/orpheusd` shows whether it
  is set. This one needs a restart of `orpheusd`.
- **Interface has no MAC address:** the default route goes through a VPN or
  another tunnel (WireGuard, OpenVPN tun). Set the LAN interface:
  `orpheusctl set ptp.interface=enp3s0`.
- **No default route:** set `ptp.interface` the same way.
- **Holding over:** the interface went down or the network changed. PTP
  starts again once the interface is back; meanwhile the clock drifts
  slowly from the other nodes.

## PTP runs but never settles

- **Firewall.** PTP needs UDP 319 and 320 in both directions; RTP needs UDP
  5004 to 5018 (one port in two per receiver, up to `[receiver] max`; see
  [configuration](configuration.md#receiver)), mDNS UDP 5353, the API
  TCP 7480, and [AES70](usage.md#aes70) TCP 7481. With firewalld:

  ```sh
  sudo firewall-cmd --permanent --add-port=319-320/udp --add-port=5004-5018/udp --add-port=7480-7481/tcp --add-service=mdns
  sudo firewall-cmd --reload
  ```

  With ufw: `sudo ufw allow 319:320/udp`, `sudo ufw allow 5004:5018/udp`,
  `sudo ufw allow 5353/udp`, `sudo ufw allow 7480:7481/tcp`. To open them to the
  home network only, name it, as in
  `sudo ufw allow from 192.168.0.0/16 to any port 319:320 proto udp`.
  Both firewalls also need IGMP let in; see
  [below](#ptp-works-for-a-few-minutes-then-stops).

  mDNS often gets through where the other ports don't, so `orpheusctl
  nodes` lists the peer all the same. If a follower's `delay` stays `-`
  and `steps` rises, the grandmaster's firewall drops its delay requests.
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
  Ethernet if you can; the servo keeps held messages out of the clock
  ([clock](clock.md#ptp)). The node warns about it (`wifi`,
  `ptp_delay` or `ptp_steps` in the [warnings](usage.md#warnings)). Turn
  off [power saving](#wi-fi-power-saving) on every node on Wi-Fi.
- **Unsteady path.** If `steps` keeps rising (warning `ptp_steps`), the
  delay to the grandmaster jumps by more than `step_threshold_us` for
  seconds at a time. Raise it, for example `[ptp] step_threshold_us =
  5000`; a cold start then slews offsets below that instead of stepping
  them. See [clock](clock.md#ptp).

## Wi-Fi power saving

A Wi-Fi client with power saving on dozes between beacons, and the access
point holds its packets until it wakes, 100 ms and more. Clocks then step and
streams drop out, whatever the buffer.

The node turns power saving off by itself, on PTP's interface and on that
of the default route, and looks again every 30 seconds. That needs
`CAP_NET_ADMIN` (`sudo setcap cap_net_bind_service,cap_net_admin=ep
path/to/orpheusd`; `just build` sets it on machines with Wi-Fi). It warns
(`power_save`) when

- it may not: the capability is missing;
- it can't: the driver doesn't let power saving be turned off. Use a wired
  link or another Wi-Fi adapter;
- something turns it on again: NetworkManager when it connects, TLP when
  the laptop goes on battery. The node turns it off again within 30
  seconds, but until then packets wait; the warning stays for 10 minutes
  after the last time.

To turn it off for good, so that nothing turns it on again: with
NetworkManager, for the connection in use:

```sh
nmcli connection modify <connection> 802-11-wireless.powersave 2
nmcli connection up <connection>
```

or for all connections, in `/etc/NetworkManager/conf.d/wifi-powersave.conf`:

```ini
[connection]
wifi.powersave = 2
```

followed by `sudo systemctl restart NetworkManager`. With TLP, set
`WIFI_PWR_ON_BAT=off` in `/etc/tlp.conf`. `sudo iw dev <interface> set
power_save off` turns it off until the next reconnect;
`iw dev <interface> get power_save` shows it.

Power saving on other devices (phones, tablets) still makes the access point
hold back multicast for everyone, PTP included. That is not something a
node can turn off.

## PTP works for a few minutes, then stops

This is the classic sign of **IGMP or MLD snooping without a querier**. A
managed switch, or a router or access point with snooping enabled, forwards
multicast only to ports that joined the group. Joins expire after a few
minutes unless a querier asks the hosts to renew them, so PTP traffic stops
reaching the nodes.

Fix it on the switch or router: enable an IGMP querier (MLD querier for
IPv6), or disable snooping. Unmanaged switches don't snoop and are not
affected.

If it happens on one node only, its **firewall drops IGMP**. The querier
asks, but the node never hears the question, so it never renews its join.
ufw does this by default (IGMP is IP protocol 2; the kernel log shows
`[UFW BLOCK] … PROTO=2` from the router about every two minutes). mDNS
keeps working, because switches and access points send link-local groups
(224.0.0.x) to every port anyway, so `orpheusctl nodes` still lists the
others. On Wi-Fi the node then gets PTP only by unicast from other
Orpheus nodes, and nothing from a grandmaster that is not one.

With ufw, add to `/etc/ufw/before.rules`, above the `COMMIT` line:

```
# allow IGMP, so that snooping switches and access points keep sending multicast
-A ufw-before-input -p igmp -j ACCEPT
```

and run `sudo ufw reload`. ufw already lets MLD in for IPv6
(`before6.rules`). With firewalld:

```sh
sudo firewall-cmd --permanent --add-protocol=igmp
sudo firewall-cmd --reload
```

## `connect` fails

- `409 … is playing another sender`: all of the peer's receivers are
  connected (`[receiver] max` on the peer). Disconnect one of them, raise
  `max` there, or take one over with `--force`. A receiver whose sender
  vanished is freed within 30 seconds once the sending node is back; if it
  stays away, use `--force`.
- `409 already connected to … as route ID`: this node already sends there
  with the same priority. If `status` shows that route as `disconnected,
  paused`, it takes the next free receiver of the peer by itself.
- `409 other senders took …'s free receivers meanwhile`: other nodes
  connected at the same moment; try again.
- `422 resolving …`: no node of that name was discovered, and the name is
  not a host name either. `orpheusctl nodes` lists the names known.
- `422 … has no receiver for L24/48000/2: …`: none of the peer's
  receivers can play the stream; the message names the first capability
  that doesn't fit, for example `channel count 16; it takes 1 to 8`. Give
  the route fewer channels (`--layout`), up to 8 for receivers that take
  only 1 ms packets.
- `502 … no answer`: the peer's API is unreachable: wrong address, the node
  is down, or a firewall blocks TCP 7480. `orpheusctl nodes` shows
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

   On the sending node, a sender shown as `(disconnected, paused)`,
   `(waiting for a free receiver)`, `(peer can't play this stream)`,
   `(idle)` or `(off)` sends nothing; see
   [reading the status](usage.md#reading-the-status). `idle` is a route
   that [claims on play](usage.md#claiming-a-receiver-on-play) while
   nothing plays into its sink; `waiting for a free receiver` means all of
   the peer's receivers play other senders. If the sink is missing
   altogether, `routes` shows `peer offline`: the peer has not answered for
   a minute.
2. If packets arrive but count as `late` or `early`, the clocks disagree. See
   the next section.
3. Check that the sender's sink actually gets audio: the application must play
   into the sink named after the peer, for example chosen in `pavucontrol`, with
   `wpctl set-default` or with `orpheusctl default`.
4. Check where the receiver plays and how loud: `orpheusctl sources` shows
   the output and volume remembered for each source, and `orpheusctl
   status` the receiver's output, `none` if its stream waits for one (see
   [outputs and volume](usage.md#outputs-and-volume)).
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
- **Bursts of `late` every 5 minutes on Wi-Fi:** background scans; see
  [below](#dropouts-every-few-minutes-on-wi-fi).
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

## Dropouts every few minutes on Wi-Fi

Bursts of `late` packets every 5 minutes, or every 30 seconds, lasting a few
seconds each, come from **background scans**. To find a better access point
to roam to, NetworkManager has the Wi-Fi interface scan every channel: every
5 minutes where it sees more than one access point for the network, and
every 30 seconds while the signal is below −65 dBm. A scan takes the
interface off its channel for about 3.5 s, and packets wait up to 120 ms
meanwhile. The PTP clock rides it out; audio does only with a link offset
above that.

`iw event -t` shows the scans as they happen (`scan started`, `new scan
results`). Opening the desktop's network menu starts one too.

NetworkManager doesn't scan in the background while a connection is locked
to one access point. For a node that doesn't move:

```sh
nmcli -f IN-USE,BSSID,SSID,CHAN,SIGNAL dev wifi   # the BSSID marked *
nmcli connection modify <connection> 802-11-wireless.bssid <BSSID>
nmcli connection up <connection>
```

The node then no longer roams: if that access point goes away, or you lock
it to the 2.4 GHz one of a router, it stays there. Undo it with
`802-11-wireless.bssid ""`. Moving the node or the access point so that the
signal is above −65 dBm at least stops the scans every 30 seconds.

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
  cannot find the node's streams by themselves. Each route has an SDP file
  at `/x-orpheus/v1/routes/{id}/sdp`, which some devices can import. Interoperability is untested; see the
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
