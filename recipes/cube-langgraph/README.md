# `cube-langgraph`

> **Status**: design stub. The orchestration script + tested integration land
> after the CubeSandbox team adds a stable snapshot-export API (see
> [`docs/INTEGRATION-CUBESANDBOX.md`](../../docs/INTEGRATION-CUBESANDBOX.md)).
> This README documents the intended workflow + what's needed from each
> side to make it work.

## What this recipe is for

You have **CubeSandbox** deployed (single-node or cluster) for normal
agent sandboxes. You want to add **forkd**'s branch-and-fan-out
primitive for the workloads where it helps — speculative parallel
exploration of stateful agents.

This recipe documents the intended workflow when both daemons run
side-by-side on the same host.

## Topology

```
┌─────────────────────────────────────────────────────────┐
│                       host                              │
│                                                         │
│  ┌─────────────────┐         ┌─────────────────────┐    │
│  │  CubeSandbox    │         │   forkd-controller  │    │
│  │  :8088          │         │   :8889             │    │
│  │  RustVMM        │         │   Firecracker       │    │
│  └────────┬────────┘         └──────────┬──────────┘    │
│           │                             │               │
│           ▼                             ▼               │
│      sandboxes for                  sandboxes that      │
│      steady-state work              need branching      │
│  (E2B-compatible API,           (forkd Python SDK,      │
│   cluster scheduling,            fork-on-write,         │
│   <60 ms cold start)             4 s pause window)      │
│                                                         │
└─────────────────────────────────────────────────────────┘
```

The agent code decides which daemon to talk to per-step based on
whether the next operation needs forking.

## Intended workflow

```python
from forkd import Controller, Sandbox
from e2b import Sandbox as E2BSandbox

# Steady-state work goes via CubeSandbox (E2B-compatible).
# Set E2B_API_URL=http://localhost:8088 in env so the SDK
# hits CubeSandbox, not the hosted E2B service.
cube = E2BSandbox.create("python-agent")
cube.commands.run("pip install langgraph langchain-openai")

# Agent does N steps of ReAct inside the CubeSandbox sandbox...
for step in range(3):
    cube.commands.run(f"python3 step{step}.py")

# Now we want to branch. CubeSandbox doesn't ship this yet, so we
# hand off to forkd:
#
#   1. Export the CubeSandbox sandbox's state (TODO: needs CubeSandbox
#      to expose a snapshot-bytes API; not present in openapi.yml as
#      of 2026-05-18)
#   2. Import into forkd as a snapshot tag
#   3. Use forkd's branch + spawn-N primitive
forkd_ctl = Controller("http://localhost:8889")

snapshot_tag = import_cube_sandbox_to_forkd(cube.id)  # pseudo, TODO
branched = forkd_ctl.branch_sandbox(snapshot_tag, tag="agent-branchpoint")

children = forkd_ctl.spawn_sandboxes(
    snapshot_tag=branched.tag,
    n=3,
    per_child_netns=True,
)

# Each child can be steered with a hint file (see
# recipes/langgraph-react for the side-channel pattern).
for child, hint in zip(children, ["thorough", "minimal", "cost"]):
    forkd_ctl.exec_sandbox(child.id, [
        "sh", "-c",
        f"printf '{hint}' > /tmp/forkd-hint.txt"
    ])
```

## What's blocking the implementation today

The pseudo-code above hinges on `import_cube_sandbox_to_forkd()`.
CubeSandbox uses **RustVMM** snapshot format; forkd uses
**Firecracker**'s. They aren't binary-compatible.

Two paths to make this real:

### Path A — CubeSandbox exposes snapshot bytes via its API

If `/sandboxes/:id/snapshot` returned the RustVMM vmstate + memory
image as a downloadable blob, forkd could:

1. Download the blob via CubeSandbox API
2. Translate the format (write a converter — non-trivial but
   mechanically possible)
3. Import into forkd's `$XDG_DATA_HOME/forkd/snapshots/`

This puts the converter on the forkd side. We're open to writing it.

### Path B — CubeSandbox adopts forkd's branch implementation

CubeSandbox's roadmap mentions "Event-level snapshot rollback (coming
soon)". When that lands, the simplest implementation is to call
forkd-controller's `/branch` endpoint behind the scenes. CubeSandbox
sandboxes that need branching would secretly be running on Firecracker.

This puts the work on the CubeSandbox side. Probably not their
preferred path (they've built their own VMM for good reasons), but
worth considering as the lowest-effort short-term option.

### Path C — agent uses both, manually

What we ship today. The agent code is aware that fork-needing steps
go to forkd, fork-not-needing steps go to CubeSandbox. No automatic
bridging; the agent operator decides per-step. Less elegant but
doesn't require either project to change.

## When to actually use this

- If you're already running CubeSandbox at scale and you've got 1-2
  agent workflows that need forking, run forkd alongside for those.
- If you're starting fresh and your workload is fork-heavy, just use
  forkd directly — CubeSandbox doesn't help here (yet).
- If you're starting fresh and your workload is steady-state +
  E2B-compatible-required, just use CubeSandbox — forkd doesn't help
  you scale across nodes (yet).

## Status / next steps

- [x] Design doc ([`docs/INTEGRATION-CUBESANDBOX.md`](../../docs/INTEGRATION-CUBESANDBOX.md))
- [x] This recipe README
- [ ] Worked example of pattern C (side-by-side, manual hand-off)
- [ ] Conversation with the CubeSandbox team on pattern A or B
- [ ] If A: format converter prototype
- [ ] If B: bridge layer in forkd-controller

If you're on the CubeSandbox team and have thoughts on which path
makes more sense — open an issue here, or ping us directly.
