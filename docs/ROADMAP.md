# sandstorm roadmap

This file tracked the v0.3 and v0.4 plans for the Firecracker runtime:
cutting the BRANCH pause window with diff snapshots, then live-fork via
`UFFDIO_WRITEPROTECT`. v0.6 replaced that runtime with a gVisor fleet,
and the plan with it. The old roadmap is in the
[`v0.5.3` tag](https://github.com/deeplethe/sandstorm/blob/v0.5.3/docs/ROADMAP.md).

For v0.6, the decisions made so far are in the
[architecture decision records](adr/README.md); tracked work is in
[GitHub issues](https://github.com/deeplethe/sandstorm/issues).
