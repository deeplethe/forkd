/**
 * Wire-level types for the forkd-controller REST API.
 *
 * Source of truth: `crates/forkd-controller/src/api.rs`. Optional
 * fields are marked optional here for v0.x compatibility — older
 * daemons may omit fields added in later releases.
 */

export interface SnapshotInfo {
  tag: string;
  dir: string;
  created_at_unix: number;
  /** Set when produced by BRANCH; the source sandbox id. */
  branched_from?: string;
  /** v0.2.5+: source-VM pause window in milliseconds during BRANCH. */
  pause_ms?: number;
  /** v0.3+: time spent in the Diff snapshot call (subset of pause_ms). */
  diff_ms?: number;
  /** v0.3+: on-disk bytes of the diff = dirty page count. */
  diff_physical_bytes?: number;
  /** v0.3+: full guest-RAM size (what a Full snapshot would have written). */
  diff_logical_bytes?: number;
  /**
   * v0.4+: live BRANCH lifecycle marker. `"writing"` while the
   * background memory copy is in flight (only seen with `wait=false`),
   * `"ready"` once the snapshot is consumable, `"failed"` if the
   * background copy hit an error.
   */
  status?: "writing" | "ready" | "failed";
}

export interface SandboxInfo {
  id: string;
  snapshot_tag: string;
  netns: string | null;
  guest_addr: string;
  created_at_unix: number;
  pid: number | null;
  memory_limit_mib: number | null;
  /** v0.3+: any BRANCH has been taken from this sandbox. */
  has_branched?: boolean;
  /** v0.3.1+: chain head for the next diff BRANCH. */
  last_branch_memory_path?: string | null;
}

export interface SpawnOptions {
  snapshot_tag: string;
  n?: number;
  per_child_netns?: boolean;
  memory_limit_mib?: number;
  /** v0.2.5+: pre-warm sandbox after restore to relocate cold-cache. */
  prewarm?: boolean;
  /**
   * v0.4+: boot the sandbox with a memfd-backed RAM region so later
   * BRANCHes from it can use `mode: "live"`. Requires kernel 5.7+ and
   * the vendored Firecracker fork (see
   * `docs/VENDORED-FIRECRACKER.md`).
   */
  live_fork?: boolean;
  /**
   * v0.4+: back the memfd with 2 MiB hugepages (`MFD_HUGETLB |
   * MFD_HUGE_2MB`). Only meaningful with `live_fork: true`. Reduces
   * TLB pressure during spawn-many and live BRANCH bulk-copy. Requires
   * non-zero `HugePages_Free` in `/proc/meminfo` — `forkd doctor`
   * checks availability. Falls back to normal 4 KiB pages with a
   * warning if the pool is exhausted.
   */
  hugepages?: boolean;
}

/**
 * Canonical BRANCH mode (Phase 7.1+).
 *
 * - `"full"` — copy entire guest RAM under pause (default for v0.x).
 * - `"diff"` — Firecracker Diff snapshot (v0.3+). Sub-second pause for
 *   idle sources; replaces the legacy `diff: true` boolean.
 * - `"live"` — UFFD_WP-based live BRANCH (v0.4+). Sub-50 ms source
 *   pause; memory streams from the running parent. Requires source
 *   booted with `live_fork: true`.
 */
export type BranchMode = "full" | "diff" | "live";

export interface BranchOptions {
  /** Optional tag for the new snapshot. Daemon generates one when unset. */
  tag?: string;
  /**
   * v0.4+ canonical mode selector. Prefer this over the legacy `diff`
   * boolean in new code. Mutually exclusive with `diff` — passing both
   * yields HTTP 400.
   */
  mode?: BranchMode;
  /**
   * **Legacy.** Equivalent to `mode: "diff"`. Kept so this SDK can
   * drive v0.3.x daemons that don't understand `mode`. Mutually
   * exclusive with `mode` server-side.
   */
  diff?: boolean;
  /**
   * v0.3+: measurement-only hook. Take a Diff snapshot inside the
   * existing Full pause to report what diff would have cost, without
   * changing semantics. Mutually exclusive with `diff` (400 if both).
   */
  measure_diff?: boolean;
  /**
   * v0.4+, only meaningful with `mode: "live"`. Default `true` blocks
   * until the background memory copy finishes and the returned
   * snapshot is `status: "ready"`. Set to `false` to return as soon
   * as the source resumes (~10 ms); snapshot reaches `status: "ready"`
   * later — poll `listSnapshots` to detect completion.
   */
  wait?: boolean;
}

export interface ExecOptions {
  args: string[];
  timeout_secs?: number;
}

export interface ExecResult {
  stdout: string;
  stderr: string;
  exit_code: number;
}

export interface EvalResult {
  result: unknown;
  error: string | null;
  exit_code: number;
}

export interface PingResult {
  /** Whatever the in-guest agent returns. Shape stable per recipe. */
  [key: string]: unknown;
}
