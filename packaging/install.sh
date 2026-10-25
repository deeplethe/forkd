#!/bin/sh
# Join this machine to a forkd master as a node.
#
# Served by the master at GET /install.sh with the two placeholders below
# filled in, so the copy you run already knows where it came from. The panel
# (and `POST /v1/node-bootstrap`) hands out the one line that fetches it.
#
# What it does: fetch the master's own binary so the versions cannot drift,
# install it, write a node unit, start it. The node dials out, so this
# machine needs no inbound port.
#
# ---------------------------------------------------------------------------
# On trust, because this runs as root and the honest answer is not "it is
# fine".
#
# The master's certificate is signed by its own CA, so plain curl refuses it
# and the one-liner cannot simply verify. What the registration token carries
# is the CA's SHA-256 (`<fingerprint>.<secret>`), and this script uses it:
# it fetches the CA, checks the fingerprint, and does every later request
# with --cacert. So the binary and the join are verified.
#
# The gap is this script itself. The first fetch cannot be verified by a
# fingerprint the script has not read yet, so a machine-in-the-middle could
# substitute THIS FILE and skip the checks below. Two ways to close it:
#
#   FORKD_CA=/path/to/ca.pem   copy the master's pki/ca.pem over first and
#                              point curl at it from the start, or
#   verify by hand             fetch with --cacert and inspect before piping
#                              to a shell.
#
# Piping an unverified script to root is a real exposure on an untrusted
# network. It is the same shape as every `curl | sh` installer, except that
# here the token at least makes everything after the first hop verifiable.
# ---------------------------------------------------------------------------
set -eu

MASTER="${FORKD_MASTER_URL:-@MASTER@}"
TOKEN="${FORKD_NODE_TOKEN:-}"
BIN="${FORKD_BIN:-/usr/local/bin/forkd-serve}"
STATE="${FORKD_STATE_DIR:-/var/lib/forkd}"
UNIT="${FORKD_UNIT:-/etc/systemd/system/forkd-node.service}"
NODE_ID="${FORKD_NODE_ID:-$(hostname)}"

die() { echo "install: $*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

[ "$(id -u)" = 0 ] || die "run this as root (the node manages netns and cgroups)"
[ -n "$TOKEN" ] || die "FORKD_NODE_TOKEN is not set; mint one with \`forkd-serve node-token\` or from the panel"
have curl || die "curl is required"

# The CA fingerprint the token pins, if it carries one.
PIN=""
case "$TOKEN" in
  *.*) PIN="${TOKEN%%.*}" ;;
esac

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

CA="${FORKD_CA:-}"
if [ -n "$CA" ]; then
  echo "install: verifying the master with $CA"
elif [ -n "$PIN" ] && [ "${#PIN}" = 64 ]; then
  # Fetch the CA unverified, then refuse it unless it hashes to what the
  # token says. An attacker who swaps the CA fails this; one who swapped
  # this script does not, which is the gap documented above.
  curl -sSfL -k -o "$TMP/ca.pem" "$MASTER/ca.pem" \
    || die "could not fetch the CA from $MASTER"
  if have openssl; then
    got="$(openssl x509 -in "$TMP/ca.pem" -outform der 2>/dev/null | openssl dgst -sha256 -hex 2>/dev/null | sed 's/.*= *//')"
  else
    die "openssl is required to check the CA fingerprint (or pass FORKD_CA)"
  fi
  [ "$got" = "$PIN" ] || die "the master's CA is $got but the token pins $PIN -- refusing"
  echo "install: CA fingerprint matches the token ($(echo "$PIN" | cut -c1-12)…)"
  CA="$TMP/ca.pem"
else
  die "this token carries no CA fingerprint; mint a new one, or pass FORKD_CA=/path/to/ca.pem"
fi

echo "install: fetching the master's own binary, so the versions cannot drift"
curl -sSfL --cacert "$CA" -o "$TMP/forkd-serve" "$MASTER/forkd-serve" \
  || die "could not fetch the binary from $MASTER"
chmod +x "$TMP/forkd-serve"
"$TMP/forkd-serve" --version >/dev/null 2>&1 \
  || die "the downloaded binary does not run on this machine"

install -D -m 0755 "$TMP/forkd-serve" "$BIN"
install -d -m 0750 "$STATE"
echo "install: $BIN $("$BIN" --version)"

for t in runsc iptables ip; do
  have "$t" || echo "install: WARNING $t is not installed; the node will refuse to start sandboxes" >&2
done

cat > "$UNIT" <<UNITEOF
[Unit]
Description=forkd node
Documentation=https://github.com/deeplethe/forkd
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
# A node only. It dials the master, so this machine needs no inbound port.
ExecStart=$BIN serve --node \\
  --master-url $MASTER \\
  --node-id $NODE_ID \\
  --state-db $STATE/node.db \\
  --pki-dir $STATE/pki \\
  --work-dir $STATE
Restart=on-failure
RestartSec=2s
TimeoutStopSec=15s

# Same capability set and hardening as packaging/systemd/forkd-serve.service;
# see the comments there for why each one is needed.
# CAP_DAC_OVERRIDE is not optional, however much it looks like it:
# mounting the per-sandbox overlay fails without it. Bisected on bm --
# with the set above minus this one cap, `mount -t overlay` returns EPERM
# and util-linux reports it as "cannot mount overlay read-only", which
# reads like a filesystem problem and is not.
CapabilityBoundingSet=CAP_NET_ADMIN CAP_NET_RAW CAP_SYS_ADMIN CAP_KILL CAP_SETUID CAP_SETGID CAP_DAC_OVERRIDE
AmbientCapabilities=CAP_NET_ADMIN CAP_NET_RAW CAP_SYS_ADMIN CAP_KILL CAP_SETUID CAP_SETGID CAP_DAC_OVERRIDE
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ProtectHome=true
# /run as well: `ip netns add` creates its namespace file under
# /run/netns, and the runsc state root defaults to /run/forkd-runsc.
# ProtectSystem=strict makes everything outside /dev, /proc and /sys
# read-only, so without this a node registers, polls, reports healthy --
# and fails every sandbox with
#     creating netns: ["ip","netns","add",...] failed
# which is what it did on bm until this line was added.
ReadWritePaths=$STATE /sys/fs/cgroup /run
ProtectControlGroups=false
ProtectKernelTunables=true
ProtectKernelModules=true
LockPersonality=true
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX AF_NETLINK
RestrictNamespaces=net mnt user pid cgroup
RestrictRealtime=true
SystemCallArchitectures=native
# @mount, explicitly. `ip netns add` bind-mounts the namespace file,
# and the node mounts an overlay per sandbox, so mount(2) is on its
# critical path. It used to be excluded via `~@privileged`, and the
# kernel's own audit log is what it looked like:
#     comm="ip" exe="/usr/bin/ip" sig=31 syscall=165
# SIGSYS on mount. The node reported only `creating netns: [...] failed:`
# with an empty stderr, because the process was killed rather than having
# anything to say. A node that cannot mount cannot run a sandbox, so the
# filter keeps @system-service and subtracts @resources only.
SystemCallFilter=@system-service @mount
SystemCallFilter=~@resources

[Install]
WantedBy=multi-user.target
UNITEOF

# The token is single-use and short-lived, so it goes in a drop-in at 0600
# rather than into the unit, which is world-readable.
mkdir -p "$UNIT.d"
printf '[Service]\nEnvironment=FORKD_NODE_TOKEN=%s\n' "$TOKEN" > "$UNIT.d/token.conf"
chmod 0600 "$UNIT.d/token.conf"

systemctl daemon-reload
systemctl enable --now "$(basename "$UNIT")"
echo "install: started $(basename "$UNIT"); it should appear in the panel within a few seconds"
echo "install: follow it with  journalctl -u $(basename "$UNIT") -f"
