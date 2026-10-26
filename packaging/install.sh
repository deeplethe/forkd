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
# Set to have the master connect to this node, e.g. 0.0.0.0:80. Unset, the
# node dials the master instead -- the right choice behind NAT, where the
# master cannot reach in.
LISTEN="${FORKD_LISTEN:-}"

die() { echo "install: $*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

[ "$(id -u)" = 0 ] || die "run this as root (the node manages netns and cgroups)"
if [ -z "$LISTEN" ]; then
  [ -n "$TOKEN" ] || die "FORKD_NODE_TOKEN is not set; mint one with \`forkd-serve node-token\` or from the panel -- or set FORKD_LISTEN=0.0.0.0:80 to have the master connect to this node instead"
fi
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

if [ -n "$LISTEN" ]; then
  CONNECT="--listen $LISTEN"
  CONNECT_NOTE="It waits on $LISTEN for the master to connect."
else
  CONNECT="--master-url $MASTER"
  CONNECT_NOTE="It dials the master, so this machine needs no inbound port."
fi

cat > "$UNIT" <<UNITEOF
[Unit]
Description=forkd node
Documentation=https://github.com/deeplethe/forkd
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
# A node only. $CONNECT_NOTE
ExecStart=$BIN serve --node \\
  $CONNECT \\
  --node-id $NODE_ID \\
  --state-db $STATE/node.db \\
  --pki-dir $STATE/pki \\
  --work-dir $STATE
Restart=on-failure
RestartSec=2s
TimeoutStopSec=15s
# A descriptor per sandbox, asleep or awake; see forkd-serve.service.
LimitNOFILE=524288

# Same capability set and hardening as packaging/systemd/forkd-serve.service;
# see the comments there for why each one is needed.
# The last two are not optional, however much they look like it, and
# neither failure names a capability.
#
#   CAP_DAC_OVERRIDE  the per-sandbox overlay mount returns EPERM, which
#                     util-linux reports as "cannot mount overlay
#                     read-only" -- a filesystem-sounding message for a
#                     permissions problem.
#   CAP_NET_BIND_SERVICE  for `--listen` on a port below 1024. The first
#                     fleet could only open 22 and 80 inbound, so the
#                     node listens on 80, and without this the bind fails
#                     with EACCES even though the process runs as root.
#   CAP_SETFCAP       runsc's gofer re-execs /proc/self/exe and fails
#                     with "operation not permitted". Found by binary
#                     search over the whole capability set on bm: the
#                     bounding set is what matters here, not the ambient
#                     one, because the gofer's exec is what needs it.
CapabilityBoundingSet=CAP_NET_ADMIN CAP_NET_RAW CAP_SYS_ADMIN CAP_KILL CAP_SETUID CAP_SETGID CAP_DAC_OVERRIDE CAP_SETFCAP CAP_NET_BIND_SERVICE
AmbientCapabilities=CAP_NET_ADMIN CAP_NET_RAW CAP_SYS_ADMIN CAP_KILL CAP_SETUID CAP_SETGID CAP_DAC_OVERRIDE CAP_SETFCAP CAP_NET_BIND_SERVICE
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
# AF_PACKET too: gVisor's network stack opens a raw packet socket on
# each sandbox interface, and without it `runsc start` fails with
#     unable to create raw socket: address family not supported by protocol
# which sounds like a kernel that is missing something and is this line.
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX AF_NETLINK AF_PACKET
# ipc and uts as well. runsc's gofer clones with CLONE_NEWIPC and
# CLONE_NEWUTS together, and this filter rejects the whole clone if
# either is missing -- adding only one of the two does not help. The
# failure is `gofer: fork/exec /proc/self/exe: operation not permitted`,
# which names neither namespaces nor systemd. Omitting them was a slip
# in writing the list, not a policy: a node needs every namespace type
# except time.
RestrictNamespaces=net mnt user pid cgroup ipc uts
RestrictRealtime=true
SystemCallArchitectures=native
# No SystemCallFilter, and that is deliberate.
#
# gVisor IS a syscall implementation: the sentry and its gofer use a set
# well outside @system-service, and it moves between runsc releases. A
# name allowlist here is a list that works until it doesn't, and the way
# it fails is a process killed with SIGSYS and an empty stderr -- the
# node reported `creating netns: [...] failed:` with nothing after the
# colon, because mount(2) was being killed rather than refused.
#
# Measured on bm by building this unit up one line at a time: every
# other setting below passes with a sandbox actually running, and
# `SystemCallFilter=@system-service @mount` fails, with or without
# @resources added back. Something gVisor needs is simply not in those
# sets, and guessing at the rest would ship a unit that breaks on the
# next runsc.
#
# What is left is not nothing: CapabilityBoundingSet, RestrictNamespaces,
# RestrictAddressFamilies, ProtectSystem=strict and the Protect* options
# below install fifteen seccomp filters between them. The allowlist is
# the one piece a sandbox runtime cannot live under.

[Install]
WantedBy=multi-user.target
UNITEOF

# The token is single-use and short-lived, so it goes in a drop-in at 0600
# rather than into the unit, which is world-readable. A listening node has
# no token: the master brings the pairing code to it.
if [ -n "$TOKEN" ]; then
  mkdir -p "$UNIT.d"
  printf '[Service]\nEnvironment=FORKD_NODE_TOKEN=%s\n' "$TOKEN" > "$UNIT.d/token.conf"
  chmod 0600 "$UNIT.d/token.conf"
fi

systemctl daemon-reload
systemctl enable --now "$(basename "$UNIT")"
if [ -n "$LISTEN" ]; then
  CODE="$STATE/pki/node/pairing-code"
  for _ in $(seq 1 50); do [ -s "$CODE" ] && break; sleep 0.2; done
  [ -s "$CODE" ] || die "the node did not write $CODE; see journalctl -u $(basename "$UNIT")"
  echo "install: listening on $LISTEN. Give the master this address and the code in"
  echo "install:   $CODE   (root only; it does not change across restarts)"
else
  echo "install: started $(basename "$UNIT"); it should appear in the panel within a few seconds"
fi
echo "install: follow it with  journalctl -u $(basename "$UNIT") -f"
