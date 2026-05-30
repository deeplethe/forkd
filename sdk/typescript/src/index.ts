/**
 * `@deeplethe/forkd` — TypeScript client for forkd.
 *
 * @example
 * ```ts
 * import { Controller, Sandbox } from '@deeplethe/forkd';
 *
 * const ctrl = new Controller({ baseUrl: 'http://127.0.0.1:8889' });
 * const snapshots = await ctrl.listSnapshots();
 *
 * // Spawn + run + cleanup in one go:
 * const out = await Sandbox.with(
 *   { snapshotTag: 'python-3-12-slim' },
 *   async (sb) => sb.exec(['python3', '-c', 'print(2+2)']),
 * );
 * console.log(out.stdout); // "4\n"
 * ```
 *
 * See {@link Controller} and {@link Sandbox} for the API surface.
 */
export { Controller, ControllerError, type ControllerOptions } from "./controller.js";
export { Sandbox } from "./sandbox.js";
export type {
  BranchMode,
  BranchOptions,
  EvalResult,
  ExecOptions,
  ExecResult,
  PingResult,
  SandboxInfo,
  SnapshotInfo,
  SpawnOptions,
} from "./types.js";
