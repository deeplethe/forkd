import { Controller, type ControllerOptions } from "./controller.js";
import type { ExecResult, SandboxInfo } from "./types.js";

/**
 * Higher-level wrapper around a single live sandbox. Mirrors the
 * shape of E2B / Daytona SDKs so existing agent code can swap in
 * forkd with minimal changes.
 *
 * Lifecycle:
 *
 * ```ts
 * import { Sandbox } from '@deeplethe/forkd';
 *
 * // Either create + own:
 * const sb = await Sandbox.create({ snapshotTag: 'python-3-12-slim' });
 * try {
 *   const r = await sb.exec(['python3', '-c', 'print(2+2)']);
 *   console.log(r.stdout); // "4\n"
 * } finally {
 *   await sb.kill();
 * }
 *
 * // Or "attach to an existing id" (e.g., after spawning N via
 * // Controller.spawnSandboxes):
 * const sb2 = new Sandbox(controller, info);
 * ```
 */
export class Sandbox {
  readonly id: string;
  readonly info: SandboxInfo;
  private readonly controller: Controller;
  private killed = false;

  constructor(controller: Controller, info: SandboxInfo) {
    this.controller = controller;
    this.info = info;
    this.id = info.id;
  }

  /**
   * Spawn one sandbox + wrap. The most common entry point.
   *
   * @example
   * ```ts
   * const sb = await Sandbox.create({
   *   snapshotTag: 'python-3-12-slim',
   *   prewarm: true,
   * });
   * ```
   */
  static async create(
    options: {
      snapshotTag: string;
      perChildNetns?: boolean;
      memoryLimitMib?: number;
      prewarm?: boolean;
    } & ControllerOptions,
  ): Promise<Sandbox> {
    const { snapshotTag, perChildNetns, memoryLimitMib, prewarm, ...ctrlOpts } =
      options;
    const ctrl = new Controller(ctrlOpts);
    const [info] = await ctrl.spawnSandboxes({
      snapshotTag,
      n: 1,
      perChildNetns,
      memoryLimitMib,
      prewarm,
    });
    if (!info) {
      throw new Error("spawn returned no sandboxes (n=1 expected)");
    }
    return new Sandbox(ctrl, info);
  }

  /** Run a subprocess in the sandbox. */
  async exec(
    args: string[],
    options: { timeoutSecs?: number } = {},
  ): Promise<ExecResult> {
    return this.controller.execCommand(this.id, args, options);
  }

  /** Evaluate Python against the warmed PID-1. */
  async eval(code: string): Promise<unknown> {
    const r = await this.controller.evalCode(this.id, code);
    if (r.error) {
      throw new Error(`eval error: ${r.error}`);
    }
    return r.result;
  }

  /** Round-trip to the in-guest agent for health + version info. */
  async ping(): Promise<Record<string, unknown>> {
    return this.controller.pingSandbox(this.id);
  }

  /**
   * Branch this sandbox into a new snapshot tag. Returns the snapshot
   * info; you can pass its `tag` to `Controller.spawnSandboxes` to
   * fan out grandchildren that inherit this sandbox's exact state.
   *
   * Opt into v0.3's diff path with `{ diff: true }` for sub-second
   * source-pause.
   */
  async branch(
    options: {
      tag?: string;
      diff?: boolean;
      measure_diff?: boolean;
    } = {},
  ): Promise<import("./types.js").SnapshotInfo> {
    return this.controller.branchSandbox(this.id, options);
  }

  /** Terminate the sandbox. Idempotent. */
  async kill(): Promise<void> {
    if (this.killed) return;
    await this.controller.killSandbox(this.id);
    this.killed = true;
  }

  /** Convenience: spawn + run callback + kill. */
  static async with<T>(
    options: Parameters<typeof Sandbox.create>[0],
    fn: (sb: Sandbox) => Promise<T>,
  ): Promise<T> {
    const sb = await Sandbox.create(options);
    try {
      return await fn(sb);
    } finally {
      await sb.kill();
    }
  }
}
