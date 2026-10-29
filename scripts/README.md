# `scripts/`

| Script | Purpose |
|---|---|
| `fleet.sh` | Stand a v0.6 fleet up across several machines, seed a template, verify, take it down |
| `dev/` | End-to-end checks against a running `sandstormd`, and `console-sim.py` for working on the console |
| `install-guest-kernel.sh` / `build-guest-kernel.sh` | Fetch or build the guest vmlinux that `--backend firecracker` boots (`--fc-kernel`) |

The rest serve the v0.5 Firecracker runtime, which is in the
[`v0.5.3` tag](https://github.com/deeplethe/sandstorm/tree/v0.5.3), and
the recipes that still build on it. `sandstormd` does not use them.

| Script | Purpose |
|---|---|
| `setup-host.sh` | One-shot: install KVM, Firecracker, Rust, KSM tuning, hugepages |
| `host-tap.sh` | Provision the host-side tap `sandstorm-tap0` that the parent VM attaches to during snapshot creation |
| `build-rootfs.sh` | Convert any Docker image into an `.ext4` rootfs with the sandstorm guest agent baked in |
| `netns-setup.sh` / `netns-teardown.sh` | Provision and remove `sandstorm-child-1 … sandstorm-child-N` network namespaces and a host bridge with MASQUERADE'd egress |

These are idempotent and require root. The operational gotchas that
went with them are in the
[v0.5.3 `scripts/README.md`](https://github.com/deeplethe/sandstorm/blob/v0.5.3/scripts/README.md).
