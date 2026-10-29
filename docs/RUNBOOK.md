# Operator runbook

This runbook described `sandstorm-controller`, the v0.5 single-host
Firecracker daemon. That daemon is gone; its runbook is in the
[`v0.5.3` tag](https://github.com/deeplethe/sandstorm/blob/v0.5.3/docs/RUNBOOK.md).

For v0.6 (`sandstormd`):

- Bring-up, one box or a fleet: the Quick start in the
  [README](../README.md#quick-start) and `scripts/fleet.sh`.
- Defaults, authentication and what is not covered:
  [SECURITY.md](SECURITY.md).
- The HTTP API, including `/metrics`: [API.md](API.md).
- systemd: [`packaging/systemd/sandstormd.service`](../packaging/systemd/sandstormd.service).
