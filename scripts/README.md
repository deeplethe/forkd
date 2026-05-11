# `scripts/`

Shell helpers used during forkd development.

## Day-by-day demos

| Script | Day | Purpose |
|---|---|---|
| `setup-host.sh` | 0 | Install KVM, Firecracker, rust, KSM tuning, hugepages |
| `day1-boot.sh` | 1 | Manually boot a vanilla Firecracker microVM |
| `day2-snapshot.sh` | 2 | Pause / snapshot / restore plumbing |
| `day3-fork.sh` | 3 | 2 children from one snapshot, verify CoW via smaps |
| `day4-network.sh` | 4 | Provision a single tap on the host for the parent VM |
| `netns-setup.sh` | 4+ | Provision N per-child network namespaces (issue #1 fix) |
| `day6-scale.sh` | 6 | Push N to 10 / 50 / 100 / 200 children |
| `build-rootfs.sh` | 5 | Build a Docker-based ext4 rootfs with apt pkgs preinstalled |
| `create-initial-issues.sh` | 7 | One-shot create the v0.0.1 GitHub issue backlog |

## Gotchas worth knowing

### bash `wait` waits for **every** background child, including firecracker

Found while writing `day6-scale.sh`. If your script does:

```bash
for i in $(seq 1 $N); do
    firecracker --api-sock $sock-$i &       # spawns long-lived firecracker
    pids+=($!)
done

for i in $(seq 1 $N); do
    {
        curl --unix-socket $sock-$i ...     # short-lived restore call
    } &
done
wait                                         # ← THIS HANGS FOREVER
```

`wait` with no argument waits for **all** background children of the
current shell. The firecracker processes never exit on their own, so
the script blocks indefinitely after the curls have already finished.

**Fix**: track the curl subshell PIDs and `wait` only on those:

```bash
restore_pids=()
for i in $(seq 1 $N); do
    {
        curl --unix-socket $sock-$i ...
    } &
    restore_pids+=($!)
done
for pid in "${restore_pids[@]}"; do wait "$pid"; done
```

This is closed as #12.

### stale unix sockets aren't cleaned by `[ -f "$p" ]`

`is_file()` in Rust and `[ -f ]` in bash return false for unix sockets,
so a glob loop that removes "files" leaves behind `*.sock` from the
previous run. The next firecracker invocation fails with
`Failed to open the API socket ... already used`.

**Fix in forkd-vmm**: sweep everything in the work_dir that isn't a
directory.

### sudo resets `$HOME` to `/root`

When `forkd fork --per-child-netns` is invoked under sudo (needed for
`ip netns exec`), `$HOME` becomes `/root` and the snapshot lookup
fails. Use `sudo -E forkd ...` to preserve the calling user's
environment.

### sudo resets `$USER`

`netns-setup.sh` defaults `USER_OWNS` to `${SUDO_USER:-$USER}` so that
the tap inside each netns is owned by the right user (not root) and
forkd can attach without elevated privileges.
