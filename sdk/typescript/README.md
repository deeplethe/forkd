# @deeplethe/forkd

TypeScript client for [forkd](https://github.com/deeplethe/forkd) — the open-source fork-on-write microVM primitive for AI agents.

```bash
npm install @deeplethe/forkd
# or pnpm add @deeplethe/forkd
```

Requires Node 18+ (uses the global `fetch`).

## Quick start

```ts
import { Controller, Sandbox } from "@deeplethe/forkd";

// Either: spawn + use + cleanup
const result = await Sandbox.with(
  { snapshotTag: "python-3-12-slim" },
  async (sb) => sb.exec(["python3", "-c", "print(2+2)"]),
);
console.log(result.stdout); // "4\n"

// Or: long-lived sandbox with BRANCH (forkd's killer move)
const ctrl = new Controller({
  baseUrl: "http://127.0.0.1:8889",
  token: process.env.FORKD_TOKEN,
});
const [source] = await ctrl.spawnSandboxes({
  snapshotTag: "langgraph-react",
});
await ctrl.execCommand(source.id, ["python3", "/opt/agent.py"]);

// BRANCH: pause source briefly, snapshot, resume. Children inherit
// source's exact state and diverge under copy-on-write.
// v0.3+: opt into diff mode for ~200 ms source-pause regardless of
// memory size (143× ceiling on 4 GiB SSD).
const checkpoint = await ctrl.branchSandbox(source.id, {
  tag: "after-warmup",
  diff: true,
});

// Fan out N children from the checkpoint.
const children = await ctrl.spawnSandboxes({
  snapshotTag: checkpoint.tag,
  n: 4,
});
```

## API surface

Surface parity with the Python SDK (`pip install forkd`):

| Python | TypeScript |
|---|---|
| `Controller.list_snapshots()` | `ctrl.listSnapshots()` |
| `Controller.delete_snapshot(tag)` | `ctrl.deleteSnapshot(tag)` |
| `Controller.spawn_sandboxes(...)` | `ctrl.spawnSandboxes({ snapshotTag, n, prewarm, ... })` |
| `Controller.list_sandboxes()` | `ctrl.listSandboxes()` |
| `Controller.get_sandbox(id)` | `ctrl.getSandbox(id)` |
| `Controller.kill_sandbox(id)` | `ctrl.killSandbox(id)` |
| `Controller.branch_sandbox(id, tag)` | `ctrl.branchSandbox(id, { tag, diff, measure_diff })` |
| `Controller.exec_command(id, args)` | `ctrl.execCommand(id, args, { timeoutSecs })` |
| `Controller.eval_code(id, code)` | `ctrl.evalCode(id, code)` |
| `Controller.ping_sandbox(id)` | `ctrl.pingSandbox(id)` |

Snake-case API field names are preserved over the wire (the daemon
expects them), but TypeScript-side argument names are camelCase.

### `Sandbox` (higher-level wrapper)

```ts
const sb = await Sandbox.create({ snapshotTag: "python-3-12-slim" });
const result = await sb.exec(["echo", "hi"]);
const value = await sb.eval("2+2");        // 4
const branch = await sb.branch({ diff: true });
await sb.kill();
```

`Sandbox.with(options, fn)` is the recommended pattern for short-lived
work — automatic cleanup even on exception.

## Configuration

```ts
new Controller({
  baseUrl: "http://127.0.0.1:8889",       // or env FORKD_URL
  token: "abc123",                         // or env FORKD_TOKEN
  timeoutMs: 60_000,                       // default
  fetch: customFetch,                      // optional (testing / older Node)
});
```

## v0.3 fast-BRANCH (diff snapshots)

forkd v0.3 added diff-snapshot BRANCH. Opt in per-call:

```ts
const branch = await ctrl.branchSandbox(sandboxId, { diff: true });
```

Measured numbers (full table in
[RESULTS-v0.3.md](https://github.com/deeplethe/forkd/blob/main/bench/pause-window/RESULTS-v0.3.md)):

- Idle 4 GiB SSD source: 29 s → 205 ms = **143×**
- Typical agent workload (30-300 MiB dirty): **6-15×**
- 5 consecutive BRANCHes (v0.3.1+): **14× aggregate**

Requires `forkd-controller >= 0.3.0`. Older daemons return 400 on `diff: true`.

## Error handling

```ts
import { ControllerError } from "@deeplethe/forkd";

try {
  await ctrl.getSandbox("sb-missing");
} catch (e) {
  if (e instanceof ControllerError && e.status === 404) {
    // sandbox doesn't exist
  }
}
```

## Testing

```bash
pnpm install
pnpm test
```

Mock fetch by passing your own implementation to the `Controller` constructor (see [`tests/controller.test.ts`](./tests/controller.test.ts)).

## See also

- [forkd-mcp](https://pypi.org/project/forkd-mcp/) — MCP server for Claude Desktop / Cursor / Cline
- [forkd-action](https://github.com/deeplethe/forkd-action) — GitHub Action
- [Python SDK](https://pypi.org/project/forkd/) — `pip install forkd`

## License

Apache-2.0.
