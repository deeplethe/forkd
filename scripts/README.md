# `scripts/`

Host-side helpers invoked from the README's quick-start and from the
operator runbook.

| Script | Purpose |
|---|---|
| `setup-host.sh` | One-shot: install KVM, Firecracker, Rust, KSM tuning, hugepages |
| `host-tap.sh` | Provision the host-side tap `sandstorm-tap0` that the parent VM attaches to during snapshot creation |
| `build-rootfs.sh` | Convert any Docker image into an `.ext4` rootfs with the sandstorm guest agent baked in |
| `netns-setup.sh` | Provision `sandstorm-child-1 … sandstorm-child-N` network namespaces and a host bridge with MASQUERADE'd egress |

All three are idempotent and require root.

## Gotchas worth knowing

### `bash` `wait` waits for every background child, including firecracker

```bash
for i in $(seq 1 $N); do
    firecracker --api-sock $sock-$i &       # spawns long-lived firecracker
done
for i in $(seq 1 $N); do
    { curl --unix-socket $sock-$i ... ; } &
done
wait                                         # never returns
```

`wait` with no argument waits for *all* background children of the
current shell. The firecracker processes never exit on their own, so
the script blocks indefinitely after the curls have already finished.

Fix: track the curl subshell PIDs and `wait` only on those.

### Stale unix sockets aren't cleaned by `[ -f "$p" ]`

`[ -f ]` (and Rust's `is_file()`) return false for unix sockets, so a
glob loop that removes "files" leaves behind `*.sock` from the previous
run. The next firecracker invocation fails with `API socket already in
use`. `sandstorm-vmm` sweeps everything in the work directory that isn't a
directory.

### `sudo` resets `$HOME` and `$USER`

When `sandstorm fork --per-child-netns` is invoked under `sudo` (needed for
`ip netns exec` and `setns(2)`), `$HOME` becomes `/root` and the
snapshot lookup fails. Use `sudo -E sandstorm ...` to preserve the calling
user's environment. `netns-setup.sh` defaults `USER_OWNS` to
`${SUDO_USER:-$USER}` so the tap inside each netns is owned by the
right user.
