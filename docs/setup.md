# First setup

A checklist for each machine that becomes a node, after the
[quick start](../README.md#quick-start). Most of it is about the network:
a node that is set up badly still plays, but out of step with the others or
with dropouts.

## On every node

1. **The node has its capabilities.** `getcap target/release/orpheusd`
   shows `cap_net_bind_service` (PTP's ports; without it the node runs on
   the [system clock](clock.md#when-ptp-cant-run)) and, on a machine with
   Wi-Fi, `cap_net_admin` (to turn power saving off). Every rebuild removes
   them; `just build` sets them again.
2. **A name.** Set `name = "Kitchen"` in `~/.config/orpheus/config.toml`
   ([configuration](configuration.md)). Otherwise the node announces its
   host name.
3. **The firewall lets Orpheus in.** UDP 319 and 320 (PTP), UDP 5004 to 5018
   (RTP), UDP 5353 (mDNS), TCP 7480 and 7481 (API, AES70), and **IGMP**.
   Without IGMP, multicast PTP stops reaching the node after a few minutes;
   see [troubleshooting](troubleshooting.md#ptp-works-for-a-few-minutes-then-stops).
   With ufw:

   ```sh
   sudo ufw allow 319:320/udp
   sudo ufw allow 5004:5018/udp
   sudo ufw allow 5353/udp
   sudo ufw allow 7480:7481/tcp
   ```

   and, for IGMP, in `/etc/ufw/before.rules` above the `COMMIT` line:

   ```
   # allow IGMP, so that snooping switches and access points keep sending multicast
   -A ufw-before-input -p igmp -j ACCEPT
   ```

   followed by `sudo ufw reload`. With firewalld:

   ```sh
   sudo firewall-cmd --permanent --add-port=319-320/udp --add-port=5004-5018/udp --add-port=7480-7481/tcp --add-service=mdns --add-protocol=igmp
   sudo firewall-cmd --reload
   ```

4. **It starts with the session.** Install the systemd user unit from
   [`orpheusd/dist/orpheusd.service`](../orpheusd/dist/orpheusd.service)
   and the [icons](../orpheusd/dist/icons/README.md).

## The network

5. **Wired where you can.** Wi-Fi works, but delays packets by milliseconds
   and now and then by 100 ms. At least the grandmaster should be wired.
6. **A wired grandmaster.** Give one wired node a lower priority, so that
   it leads the clock even if a node on Wi-Fi starts first:

   ```toml
   [ptp]
   priority1 = 200
   ```

   A node on Wi-Fi that may become grandmaster warns
   (`wifi_grandmaster`). A dedicated AES67 or Dante grandmaster wins
   anyway.
7. **Snooping switches have a querier.** If a managed switch or the router
   snoops IGMP, one of them must also act as IGMP querier (MLD querier for
   IPv6). Unmanaged switches don't snoop.

## Each node on Wi-Fi

8. **Power saving off for good.** The node turns it off itself, but
   NetworkManager or TLP may turn it on again; it then warns
   (`power_save`). On a laptop that should save battery, set
   `[wifi] power_save = "auto"` instead. See
   [troubleshooting](troubleshooting.md#wi-fi-power-saving).
9. **No background scans.** NetworkManager scans all channels every 5
   minutes, and every 30 s while the signal is weak, so that it can roam.
   Each scan takes the node off its channel for about 3.5 s, and packets
   wait up to 120 ms meanwhile. A node that doesn't move can stay with one
   access point; see
   [troubleshooting](troubleshooting.md#dropouts-every-few-minutes-on-wi-fi).
10. **A good signal.** `iw dev <interface> link` shows it; below about
    −65 dBm NetworkManager scans every 30 s, and below about −70 dBm the
    access point sends at low rates. The node names the signal in its
    `jitter`, `ptp_delay` and `ptp_steps` warnings. 5 GHz near the access
    point is best.
11. **More link offset.** The default `link_offset_ms = 10` is for wired
    networks. Over Wi-Fi, 20 to 30 ms rides out most delays, but not
    the up to 120 ms of a scan. Set the same value on all receivers of a
    stream.

## Check

12. `orpheusctl nodes` lists the other nodes, each with its link (`wired`
    or `Wi-Fi`).
13. `orpheusctl status` on each node shows `PTP, settled` within a minute
    of the start, no warnings, and a `steps` count that stops rising after
    the first few.
14. Connect two nodes, play something for a few minutes, and watch
    `orpheusctl status --watch` on the receiver: `late`, `lost` and
    `underruns` should stay at 0 most of the time. If they don't, see
    [dropouts and clicks](troubleshooting.md#dropouts-and-clicks).
