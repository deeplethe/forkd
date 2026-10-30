# @deeplethe/sandstorm

TypeScript client for [sandstorm](https://github.com/deeplethe/sandstorm) — the open-source fork-on-write microVM primitive for AI agents.

```bash
npm install @deeplethe/sandstorm
# or pnpm add @deeplethe/sandstorm
```

Requires Node 20+. (The code only needs the global `fetch`, which Node 18
had too; the floor is 20 because that is what the SDK is tested on -- Node
18 has been end-of-life since April 2025.)

## Quick start

```ts
import { Controller, Sandbox } from "@deeplethe/sandstorm";

// Either: spawn + use + cleanup
const result = await Sandbox.with(
  { snapshotTag: "python-3-12-slim" },
  async (sb) => sb.exec(["python3", "-c", "print(2+2)"]),
);
console.log(result.stdout); // "4\n"

// Or: long-lived sandbox with BRANCH (sandstorm's killer move)
const ctrl = new Controller({
  baseUrl: "http://127.0.0.1:8889",
  token: process.env.SANDSTORM_TOKEN,
});
const [source] = await ctrl.spawnSandboxes({
  snapshotTag: "langgraph-react",
});
await ctrl.execCommand(source.id, ["python3", "/opt/agent.py"], { sandboxToken: source.token });

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

Surface parity with the Python SDK (`pip install sandstorm-sdk`):

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

Keep the creation response, including its `token`: v0.6 requires this sandbox
token in addition to the API Bearer credential for exec/eval/ping. `Sandbox.create`
forwards it automatically, as does `new Sandbox(ctrl, info)` when `info` is the
creation result. GET/LIST responses do not contain it. Older servers may omit
it; missing or incorrect tokens still produce the server's authentication error.

`Sandbox.with(options, fn)` is the recommended pattern for short-lived
work — automatic cleanup even on exception.

## Configuration

```ts
new Controller({
  baseUrl: "http://127.0.0.1:8889",       // or env SANDSTORM_URL
  token: "abc123",                         // or env SANDSTORM_TOKEN
  timeoutMs: 60_000,                       // default
  fetch: customFetch,                      // optional (testing / older Node)
});
```

## v0.3 fast-BRANCH (diff snapshots)

sandstorm v0.3 added diff-snapshot BRANCH. Opt in per-call:

```ts
const branch = await ctrl.branchSandbox(sandboxId, { diff: true });
```

Measured numbers (full table in
[RESULTS-v0.3.md](https://github.com/deeplethe/sandstorm/blob/v0.5.3/bench/pause-window/RESULTS-v0.3.md)):

- Idle 4 GiB SSD source: 29 s → 205 ms = **143×**
- Typical agent workload (30-300 MiB dirty): **6-15×**
- 5 consecutive BRANCHes (v0.3.1+): **14× aggregate**

Requires `sandstorm-controller >= 0.3.0`. Older daemons return 400 on `diff: true`.

## Error handling

```ts
import { ControllerError } from "@deeplethe/sandstorm";

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

- [sandstorm-mcp](https://pypi.org/project/sandstorm-mcp/) — MCP server for Claude Desktop / Cursor / Cline
- [sandstorm-action](https://github.com/deeplethe/sandstorm-action) — GitHub Action
- [Python SDK](https://pypi.org/project/sandstorm-sdk/) — `pip install sandstorm-sdk`

## License

Apache-2.0.
