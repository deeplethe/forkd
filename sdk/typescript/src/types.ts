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
}

export interface BranchOptions {
  /** Optional tag for the new snapshot. Daemon generates one when unset. */
  tag?: string;
  /**
   * v0.3+: use Firecracker Diff snapshot mode. Source pause window
   * collapses to the diff write only (~200 ms idle source, 6-15×
   * speedup on typical agent workloads, 143× ceiling on 4 GiB SSD).
   * Multi-BRANCH supported in v0.3.1+ via the previous-output chain.
   */
  diff?: boolean;
  /**
   * v0.3+: measurement-only hook. Take a Diff snapshot inside the
   * existing Full pause to report what diff would have cost, without
   * changing semantics. Mutually exclusive with `diff` (400 if both).
   */
  measure_diff?: boolean;
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
