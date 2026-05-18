# `coding-agent-fork`

The "**why not just parallel-prompt the LLM 3 times?**" rebuttal demo.

A small coding agent inside the source sandbox builds a Python
package, runs a failing test (populating `__pycache__/`), and
writes a 50 MiB synthetic "vendored.bin" representing things a
real agent accumulates (pip caches, downloaded weights, compiled
extensions). We `forkd branch` the source. Three grandchildren
each apply a different fix strategy. The state evidence shows:

- The 50 MiB `vendored.bin` is **byte-identical** across all
  4 agents (md5 verified).
- The `__pycache__/` directory is **byte-identical** at the moment
  of branch — after each child re-imports, its own `__pycache__/`
  diverges (because the source file changed).
- Each child's `mathy/__init__.py` after applying its strategy
  is **different** — minimal sed, full rewrite, skip-the-tests.
- Test outcomes diverge: the first two strategies actually fix
  the bug; the "skip" strategy backfires when one of the
  expected-failure tests accidentally passes.

This is the *filesystem* analog of the
[langgraph-react demo](../langgraph-react/DEMO.md)'s
*conversation-history* fork. Same primitive; different state.

## Why parallel prompts can't replicate this

To run 3 "fix attempts" via API-only parallelism, each request
would have to carry the **entire** `/workspace` directory — files,
binary cache, populated `__pycache__/`. That's:

- 50 MiB synthetic vendored data
- Several KiB of compiled `.pyc` bytecode
- ~20 source files of various sizes

Not just wasteful (3× the bytes uploaded). **Technically impossible**
above ~50 KiB on most LLM APIs. And the LLM doesn't even understand
binary blobs.

forkd's BRANCH primitive sidesteps this entirely: the children
inherit the source's address space copy-on-write. The 50 MiB
appears in each child the moment they're spawned, no transfer
needed.

## Run the demo

Prereqs: a running forkd-controller daemon, the `langgraph`
snapshot tag (or any python3-capable tag — `setup-source.sh`
only needs stdlib).

```bash
export FORKD_URL=http://127.0.0.1:8889
export FORKD_TOKEN=$(cat /etc/forkd/token)
bash recipes/coding-agent-fork/demo.sh
```

Artifacts land in `recipes/coding-agent-fork/results/<unix-ts>/`:

- `summary.md` — per-agent state evidence + divergent code
- `summary.json` — machine-readable
- `branch.json` — daemon's BRANCH response with pause_ms
- `state-evidence.txt` — raw md5s
- `{source,minimal,rewrite,skip}-init-py.txt` — each agent's
  `mathy/__init__.py` after their strategy ran
- `{source,minimal,rewrite,skip}-agent.log` — full per-agent
  shell log incl. unittest output

A real run (2026-05-19, 3.3 s pause) is committed under
[`results-2026-05-19/`](./results-2026-05-19/) — see
[`results-2026-05-19/summary.md`](./results-2026-05-19/summary.md)
for the divergent code + test outcomes.

## Three strategies, one shared starting point

| Strategy | What it does | Test outcome |
|---|---|---|
| `minimal.sh` | One-line `sed` to flip `a - b` → `a + b` | ✅ passed |
| `rewrite.sh` | Full function rewrite with type-checks | ✅ passed |
| `skip.sh` | Decorate tests with `@unittest.expectedFailure` | ❌ failed (one test unexpectedly passed and broke the contract) |

The pedagogical bonus: the lazy `skip` strategy backfires because
`test_add_zero` (`0 - 0 == 0`) accidentally passes despite the bug —
unittest flags this as an "unexpected success" and fails the suite.
This is the kind of thing only branch-and-compare reveals.

## Why this is in `/tmp/workspace`, not `/workspace`

`/tmp` is mounted as **tmpfs** inside the guest by
[`rootfs-init/forkd-init.sh`](../../rootfs-init/forkd-init.sh) —
each VM has its own per-instance RAM-backed `/tmp`. BRANCH captures
this in `memory.bin`. Children inherit byte-identical tmpfs
contents on restore.

Using `/workspace` on the rootfs ext4 would NOT work: the rootfs
file is shared (loop-mounted) across all sandboxes from the same
snapshot. Three children writing concurrently to the same on-disk
inode would corrupt the journal.

If you're writing your own forkd recipe and need writable shared
state across forks, **always put it under `/tmp`**.

## Limitations

- The 50 MiB blob is synthetic. A real "coding agent" might have
  GiB of state (pip's `site-packages/`, downloaded model weights,
  vendored sources). forkd's pause window scales roughly with
  memory image size (see [`bench/pause-window/RESULTS-v0.2.md`](../../bench/pause-window/RESULTS-v0.2.md));
  testing with realistic blob sizes is on the v0.3 roadmap.
- This demo uses shell scripts as the "agent". A real
  coding agent calls an LLM. The langgraph-react recipe shows the
  LLM path; this one shows the filesystem-state path. Both
  primitives compose.
