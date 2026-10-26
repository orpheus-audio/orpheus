# Profiles: dev (debug build, fastest to build), release (default), optimized
# (whole-program optimization for this machine's CPU; slow to build, and the
# binaries do not run on other machines).

# List the recipes.
default:
    @just --list

# Build orpheusd and orpheusctl and give orpheusd its capabilities.
build profile="release":
    {{ if profile == "optimized" { "RUSTFLAGS='-C target-cpu=native'" } else { "" } }} cargo build --profile {{profile}} -p orpheusd -p orpheusctl
    @just setcap {{profile}}

# Build and run orpheusd.
run profile="release" *args: (build profile)
    target/{{ if profile == "dev" { "debug" } else { profile } }}/orpheusd {{args}}

# Let orpheusd bind the PTP ports 319 and 320 and, on a machine with Wi-Fi,
# turn Wi-Fi power saving off, unless it already can.
setcap profile="release":
    #!/bin/sh
    set -eu
    bin=target/{{ if profile == "dev" { "debug" } else { profile } }}/orpheusd
    caps=""
    if [ "$(cat /proc/sys/net/ipv4/ip_unprivileged_port_start)" -gt 319 ]; then
        caps="cap_net_bind_service"
    fi
    if ls -d /sys/class/net/*/phy80211 >/dev/null 2>&1; then
        caps="${caps:+$caps,}cap_net_admin"
    fi
    have=$(getcap "$bin")
    missing=""
    for c in $(echo "$caps" | tr , ' '); do
        echo "$have" | grep -q "$c" || missing=yes
    done
    [ -z "$missing" ] && exit 0
    echo "setcap needs sudo: PTP binds the privileged ports 319 and 320, and the node turns Wi-Fi power saving off"
    sudo setcap "$caps=ep" "$bin"

# Format, lint and test the workspace.
check:
    cargo fmt --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace
