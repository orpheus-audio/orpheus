# Development

## Building

```sh
cargo build --workspace
cargo test --workspace
```

The [justfile](../justfile) has shortcuts. `just build [profile]` builds
`orpheusd` and `orpheusctl` and runs `setcap` on `orpheusd` if it needs it
(asking for sudo); `just run [profile]` also starts it. `just check` runs
rustfmt, clippy and the tests. The profiles:

- `dev`: a debug build, the fastest to rebuild; for trying out changes.
- `release` (the default): for normal use.
- `optimized`: adds fat LTO, one codegen unit and `-C target-cpu=native`;
  for running a node for long. It takes several times as long to build as
  `release`, and its binaries run only on CPUs like the one that built them,
  so build it on each machine rather than copying it. It lives in
  `target/optimized`, so it does not invalidate the other builds.

The PipeWire bindings need the PipeWire development headers and clang.

`cargo build --workspace --examples` builds only the examples, not
`orpheusd` and `orpheusctl`.

## Tests

- Unit tests live next to the code.
- `orpheus-engine/tests/rtp_route.rs` runs sender → UDP → receiver on
  localhost, with a fake audio backend in place of PipeWire.
- `drift_is_absorbed` in the same file is ignored by default. It runs two
  minutes with the media clock 500 ppm fast against the sound card and checks
  that ASRC holds the latency:

  ```sh
  cargo test -p orpheus-engine --test rtp_route -- --ignored
  ```

- `orpheusd/src/api.rs` checks that the OpenAPI document lists every route and
  that every schema reference resolves.
- The servo tests in `orpheus-clock/src/ptp.rs` run the PTP filter against a
  simulated clock and path. `servo_replays_recorded_wifi`, ignored by
  default, replays waits recorded on a real network instead: a directory
  with `trace-sync.txt` and `trace-delay.txt`, one `time wait` in seconds
  per line (the wait being how much later than the fastest the message
  arrived):

  ```sh
  ORPHEUS_TRACE=path/to/traces cargo test -p orpheus-clock servo_replays -- --ignored --nocapture
  ```

## Examples

| Example | Purpose |
|---|---|
| `orpheus-clock/examples/ptp_follow.rs` | Runs only the PTP clock and prints its status once a second: `ptp_follow <interface> [skew_ppm] [offset_ms] [seconds] [priority1]`. |
| `orpheus-pipewire/examples/latency_probe.rs` | Measures the latency of a PipeWire path by playing impulses into one node and detecting them on the monitor of a sink. |

## Two nodes on one machine

Two nodes on one machine need separate network namespaces: each needs its own
ports, and PTP multicast doesn't loop back. Without namespaces, two nodes can
still share one machine with PTP disabled: give them separate `ORPHEUS_HOME`
directories and different `[api] listen` and `[receiver] port` values.
The receivers take every second port from `port` on, so keep the two
`port` values at least `2 × max` apart, for example 5004 and 5104.

The debug variables `ORPHEUS_DEBUG_CLOCK_SKEW_PPM` and
`ORPHEUS_DEBUG_CLOCK_OFFSET_MS` skew a node's view of the system clock, so
that one machine can play two nodes whose clocks disagree. The real kernel
clock is shared, so this tests the servo and ASRC, not real crystal drift or
network jitter. Use two machines for that.

Because the kernel clock is shared, the true clock error is known on one
machine: a node's PTP time minus the unskewed system time (when the
grandmaster runs unskewed). That is how the servo was tuned. Network jitter
can be simulated with `tc qdisc ... netem delay 50us 50us`, but only between
the timestamps: software TX timestamps are taken in the driver, after the
qdisc, so netem on a node's own veth is invisible to PTP. Put a bridge in a
third namespace between the nodes and add netem to the bridge ports.

`ORPHEUS_DEBUG_WIRELESS=veth-b` (or `*`) makes a node treat those
interfaces as Wi-Fi, to test the [network warnings](usage.md#warnings);
`netem delay 6ms 4ms` on a bridge port raises the receiver's `jitter`.

## NMOS conformance

The [AMWA NMOS Testing Tool](https://github.com/AMWA-TV/nmos-testing) checks
the NMOS APIs. Clone it outside the synced folders, install its
`requirements.txt` into a virtual environment, start a node with a sender
(the suites need resources to test), and run, for example:

```sh
python3 nmos-test.py suite IS-04-01 --host 10.99.0.2 --port 7480 \
  --version v1.3 --output is-04-01.json
python3 nmos-test.py suite IS-05-01 --host 10.99.0.2 --port 7480 \
  --version v1.1 --output is-05-01.json
python3 nmos-test.py suite IS-05-02 --host 10.99.0.2 10.99.0.2 \
  --port 7480 7480 --version v1.3 v1.1 --output is-05-02.json
```

On first start the tool clones the NMOS specifications from GitHub, so a
namespace without internet needs one run outside first. IS-05-01
and IS-05-02 pass completely. With IS-04-01 all Node API tests pass; the
registration tests fail, as expected, because nodes don't register with a
registry. The tool decodes TXT records as ASCII, which
is why the name is not in the TXT record.

## OpenAPI

```sh
cargo run -p orpheusd -- --openapi > openapi.json
```

The handlers in `orpheusd/src/api.rs` carry their own OpenAPI annotations
(utoipa), so the document follows the code.

## Conventions

- Rust edition 2024; `cargo fmt` and `cargo clippy --workspace`.
- Platform-independent code goes into `orpheus-core` or `orpheus-engine`, so
  it can be shared with the ESP32 firmware and other platforms later.
- Realtime callbacks don't allocate, lock or make syscalls.
- Update these docs at the end of each milestone.
