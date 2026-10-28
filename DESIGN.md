# Design

The current design is [DESIGN-v0.6.md](DESIGN-v0.6.md) (Chinese): a
gVisor (`runsc`) sandbox fleet, one master and many nodes, with hot,
frozen, snapshot and detached tiers. All of it is in
[`crates/sandstorm-core`](crates/sandstorm-core).

This file used to describe the v0.5 runtime: Firecracker microVMs whose
children shared the parent's memory through `MAP_PRIVATE` copy-on-write,
driven by the `sandstorm` CLI and `sandstorm-controller`. That code and
its design notes (`DESIGN-v0.4*.md`, `DESIGN-v0.5-diff-snapshot-chains.md`,
`docs/design/`) are in the
[`v0.5.3` tag](https://github.com/deeplethe/sandstorm/blob/v0.5.3/DESIGN.md).
