import type {
  BranchOptions,
  EvalResult,
  ExecOptions,
  ExecResult,
  PingResult,
  SandboxInfo,
  SnapshotInfo,
  SpawnOptions,
} from "./types.js";

/**
 * Raised on non-2xx responses from the daemon.
 *
 * Inspect `status` and `body` to distinguish 404 (sandbox/snapshot
 * missing) from 409 (tag collision) from 500 (internal).
 */
export class ControllerError extends Error {
  readonly status: number;
  readonly body: unknown;
  readonly url: string;

  constructor(status: number, body: unknown, url: string) {
    const snippet =
      typeof body === "string" ? body : JSON.stringify(body);
    super(`controller ${url}: HTTP ${status}: ${snippet}`);
    this.name = "ControllerError";
    this.status = status;
    this.body = body;
    this.url = url;
  }
}

export interface ControllerOptions {
  /**
   * Daemon base URL. Defaults to env `FORKD_URL` then
   * `http://127.0.0.1:8889`.
   */
  baseUrl?: string;
  /**
   * Bearer token. Defaults to env `FORKD_TOKEN`. Required when the
   * daemon was started with `--token-file`.
   */
  token?: string;
  /**
   * Per-request timeout in milliseconds. Default 60_000.
   * Branching a large source can take several seconds before v0.3's
   * diff mode kicks in.
   */
  timeoutMs?: number;
  /**
   * Custom fetch implementation. Defaults to global `fetch`. Provide
   * `undici`'s fetch in Node test scenarios that want to record
   * traffic, or polyfill for older Node.
   */
  fetch?: typeof fetch;
}

/**
 * Client for the forkd-controller daemon's REST API.
 *
 * @example
 * ```ts
 * import { Controller } from '@deeplethe/forkd';
 * const ctrl = new Controller({
 *   baseUrl: 'http://127.0.0.1:8889',
 *   token: process.env.FORKD_TOKEN,
 * });
 * const snapshots = await ctrl.listSnapshots();
 * const [sb] = await ctrl.spawnSandboxes({ snapshotTag: 'python-3-12-slim' });
 * const result = await ctrl.execCommand(sb.id, ['python3', '-c', 'print(2+2)']);
 * const branch = await ctrl.branchSandbox(sb.id, { diff: true });
 * await ctrl.killSandbox(sb.id);
 * ```
 */
export class Controller {
  readonly baseUrl: string;
  readonly token: string | undefined;
  readonly timeoutMs: number;
  private readonly fetchImpl: typeof fetch;

  constructor(opts: ControllerOptions = {}) {
    const envUrl =
      typeof process !== "undefined" ? process.env.FORKD_URL : undefined;
    const envToken =
      typeof process !== "undefined" ? process.env.FORKD_TOKEN : undefined;
    this.baseUrl = (opts.baseUrl ?? envUrl ?? "http://127.0.0.1:8889").replace(
      /\/+$/,
      "",
    );
    this.token = opts.token ?? envToken ?? undefined;
    this.timeoutMs = opts.timeoutMs ?? 60_000;
    this.fetchImpl = opts.fetch ?? globalThis.fetch;
    if (typeof this.fetchImpl !== "function") {
      throw new Error(
        "Controller: no fetch implementation. Node 18+ ships fetch globally; otherwise pass `fetch` in options.",
      );
    }
  }

  // --- snapshots ----------------------------------------------------

  async listSnapshots(): Promise<SnapshotInfo[]> {
    return this.request<SnapshotInfo[]>("GET", "/v1/snapshots");
  }

  async deleteSnapshot(tag: string): Promise<void> {
    await this.request<null>("DELETE", `/v1/snapshots/${encodeURIComponent(tag)}`);
  }

  // --- sandboxes ----------------------------------------------------

  /**
   * Fork N children from a registered snapshot.
   *
   * @param options.snapshotTag    snake_case in the wire format,
   *                               camelCase here.
   * @param options.prewarm        v0.2.5+. Relocates the cold-cache
   *                               penalty from the first BRANCH to
   *                               sandbox-creation time.
   * @param options.liveFork       v0.4+. Boot with memfd-backed RAM so
   *                               later BRANCHes from this sandbox can
   *                               use `mode: "live"`. Requires kernel
   *                               5.7+ and the vendored Firecracker
   *                               fork.
   */
  async spawnSandboxes(options: {
    snapshotTag: string;
    n?: number;
    perChildNetns?: boolean;
    memoryLimitMib?: number;
    prewarm?: boolean;
    liveFork?: boolean;
    /** v0.4+: back the memfd with 2 MiB hugepages. Only meaningful with `liveFork: true`. */
    hugepages?: boolean;
  }): Promise<SandboxInfo[]> {
    const body: SpawnOptions = {
      snapshot_tag: options.snapshotTag,
      n: options.n ?? 1,
      per_child_netns: options.perChildNetns ?? false,
    };
    if (options.memoryLimitMib !== undefined) {
      body.memory_limit_mib = options.memoryLimitMib;
    }
    if (options.prewarm !== undefined) {
      body.prewarm = options.prewarm;
    }
    if (options.liveFork !== undefined) {
      body.live_fork = options.liveFork;
    }
    if (options.hugepages !== undefined) {
      body.hugepages = options.hugepages;
    }
    return this.request<SandboxInfo[]>("POST", "/v1/sandboxes", body);
  }

  async listSandboxes(): Promise<SandboxInfo[]> {
    return this.request<SandboxInfo[]>("GET", "/v1/sandboxes");
  }

  async getSandbox(sandboxId: string): Promise<SandboxInfo> {
    return this.request<SandboxInfo>(
      "GET",
      `/v1/sandboxes/${encodeURIComponent(sandboxId)}`,
    );
  }

  async killSandbox(sandboxId: string): Promise<void> {
    await this.request<null>(
      "DELETE",
      `/v1/sandboxes/${encodeURIComponent(sandboxId)}`,
    );
  }

  /**
   * Branch a running sandbox into a new snapshot.
   *
   * Pauses the source briefly, snapshots, resumes. Pause window
   * depends on `options.mode`:
   *
   * - `"full"` (default): 0.5-8 s, whole guest RAM written.
   * - `"diff"` (v0.3+): ~200 ms idle source, 6-15× speedup on typical
   *   agent workloads, 143× ceiling on 4 GiB SSD.
   * - `"live"` (v0.4+): sub-50 ms; memory streams from the running
   *   parent via UFFD_WP. Requires source booted with
   *   `liveFork: true`. Combine with `wait: false` to return after
   *   the source resumes (~10 ms) without waiting on the background
   *   copy.
   *
   * The legacy `options.diff` boolean still works for v0.3.x daemon
   * compat but is mutually exclusive with `options.mode` server-side.
   *
   * Returns a {@link SnapshotInfo}; pass its `tag` back into
   * {@link spawnSandboxes} to fan out grandchildren.
   */
  async branchSandbox(
    sandboxId: string,
    options: BranchOptions = {},
  ): Promise<SnapshotInfo> {
    const body: BranchOptions = {};
    if (options.tag !== undefined) body.tag = options.tag;
    // Prefer canonical `mode` when set; fall back to legacy `diff`
    // so older daemons keep working unchanged.
    if (options.mode !== undefined) {
      body.mode = options.mode;
    } else if (options.diff) {
      body.diff = true;
    }
    if (options.measure_diff) body.measure_diff = true;
    // `wait: true` is the daemon default; only send when the caller
    // opted into fire-and-forget so the body stays minimal against
    // daemons that don't recognize the field.
    if (options.wait === false) body.wait = false;
    return this.request<SnapshotInfo>(
      "POST",
      `/v1/sandboxes/${encodeURIComponent(sandboxId)}/branch`,
      body,
    );
  }

  async execCommand(
    sandboxId: string,
    args: string[],
    options: { timeoutSecs?: number } = {},
  ): Promise<ExecResult> {
    const body: ExecOptions = {
      args,
      timeout_secs: options.timeoutSecs ?? 30,
    };
    return this.request<ExecResult>(
      "POST",
      `/v1/sandboxes/${encodeURIComponent(sandboxId)}/exec`,
      body,
    );
  }

  async evalCode(sandboxId: string, code: string): Promise<EvalResult> {
    return this.request<EvalResult>(
      "POST",
      `/v1/sandboxes/${encodeURIComponent(sandboxId)}/eval`,
      { code },
    );
  }

  async pingSandbox(sandboxId: string): Promise<PingResult> {
    return this.request<PingResult>(
      "POST",
      `/v1/sandboxes/${encodeURIComponent(sandboxId)}/ping`,
    );
  }

  // --- internals ----------------------------------------------------

  private async request<T>(
    method: string,
    path: string,
    body?: unknown,
  ): Promise<T> {
    const url = `${this.baseUrl}${path}`;
    const headers: Record<string, string> = {};
    if (body !== undefined) headers["content-type"] = "application/json";
    if (this.token) headers["authorization"] = `Bearer ${this.token}`;
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), this.timeoutMs);
    try {
      const resp = await this.fetchImpl(url, {
        method,
        headers,
        body: body !== undefined ? JSON.stringify(body) : undefined,
        signal: controller.signal,
      });
      if (!resp.ok) {
        let parsed: unknown;
        const text = await resp.text();
        try {
          parsed = text ? JSON.parse(text) : {};
        } catch {
          parsed = text;
        }
        throw new ControllerError(resp.status, parsed, url);
      }
      // DELETE returns 204 / empty; tolerate.
      const text = await resp.text();
      if (!text) return null as T;
      return JSON.parse(text) as T;
    } finally {
      clearTimeout(timer);
    }
  }
}
