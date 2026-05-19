import { describe, expect, it, vi } from "vitest";
import { Controller, ControllerError } from "../src/controller.js";

/**
 * Mock fetch helper. Returns a fetch impl that records calls and
 * replies with the given status + body for the next request.
 */
function mockFetch(replies: Array<{ status: number; body: unknown }>) {
  const calls: Array<{ url: string; init: RequestInit | undefined }> = [];
  let i = 0;
  const f: typeof fetch = (async (input, init) => {
    calls.push({ url: String(input), init });
    const reply = replies[i++];
    if (!reply) {
      throw new Error(`unexpected fetch call: ${input}`);
    }
    const text = typeof reply.body === "string" ? reply.body : JSON.stringify(reply.body);
    return {
      ok: reply.status >= 200 && reply.status < 300,
      status: reply.status,
      text: async () => text,
    } as Response;
  }) as typeof fetch;
  return { fetch: f, calls };
}

describe("Controller", () => {
  it("listSnapshots returns parsed JSON", async () => {
    const { fetch: f, calls } = mockFetch([
      { status: 200, body: [{ tag: "py", dir: "/x", created_at_unix: 1 }] },
    ]);
    const c = new Controller({ baseUrl: "http://test/", fetch: f });
    const result = await c.listSnapshots();
    expect(result).toEqual([{ tag: "py", dir: "/x", created_at_unix: 1 }]);
    expect(calls[0]!.url).toBe("http://test/v1/snapshots");
    expect((calls[0]!.init as RequestInit).method).toBe("GET");
  });

  it("spawnSandboxes serializes camelCase → snake_case", async () => {
    const { fetch: f, calls } = mockFetch([
      { status: 201, body: [{ id: "sb-1", snapshot_tag: "py" }] },
    ]);
    const c = new Controller({ fetch: f });
    await c.spawnSandboxes({
      snapshotTag: "py",
      n: 3,
      perChildNetns: true,
      memoryLimitMib: 512,
      prewarm: true,
    });
    const init = calls[0]!.init as RequestInit;
    const body = JSON.parse(init.body as string);
    expect(body).toEqual({
      snapshot_tag: "py",
      n: 3,
      per_child_netns: true,
      memory_limit_mib: 512,
      prewarm: true,
    });
  });

  it("branchSandbox passes diff option through", async () => {
    const { fetch: f, calls } = mockFetch([
      { status: 201, body: { tag: "b1", dir: "/x", created_at_unix: 1 } },
    ]);
    const c = new Controller({ fetch: f });
    await c.branchSandbox("sb-1", { tag: "b1", diff: true });
    const body = JSON.parse(
      (calls[0]!.init as RequestInit).body as string,
    );
    expect(body).toEqual({ tag: "b1", diff: true });
  });

  it("raises ControllerError on non-2xx with JSON body", async () => {
    // Two replies because we call getSandbox twice (one rejects.toBe,
    // one in a try/catch to inspect status+body).
    const { fetch: f } = mockFetch([
      { status: 404, body: { error: "not found" } },
      { status: 404, body: { error: "not found" } },
    ]);
    const c = new Controller({ fetch: f });
    await expect(c.getSandbox("sb-missing")).rejects.toBeInstanceOf(
      ControllerError,
    );
    try {
      await c.getSandbox("sb-missing");
      throw new Error("should have thrown");
    } catch (e) {
      expect(e).toBeInstanceOf(ControllerError);
      expect((e as ControllerError).status).toBe(404);
      expect((e as ControllerError).body).toEqual({ error: "not found" });
    }
  });

  it("authorization header set when token provided", async () => {
    const { fetch: f, calls } = mockFetch([{ status: 200, body: [] }]);
    const c = new Controller({ token: "abc123", fetch: f });
    await c.listSnapshots();
    const headers = (calls[0]!.init as RequestInit).headers as Record<
      string,
      string
    >;
    expect(headers.authorization).toBe("Bearer abc123");
  });

  it("FORKD_URL env honored as default baseUrl", async () => {
    const orig = process.env.FORKD_URL;
    process.env.FORKD_URL = "http://from-env";
    try {
      const c = new Controller();
      expect(c.baseUrl).toBe("http://from-env");
    } finally {
      if (orig === undefined) delete process.env.FORKD_URL;
      else process.env.FORKD_URL = orig;
    }
  });

  it("DELETE returns void / does not parse empty body", async () => {
    const { fetch: f } = mockFetch([{ status: 204, body: "" }]);
    const c = new Controller({ fetch: f });
    // Should not throw on empty body.
    await c.killSandbox("sb-1");
  });

  it("encodes sandbox id in URL path", async () => {
    const { fetch: f, calls } = mockFetch([{ status: 200, body: { id: "x" } }]);
    const c = new Controller({ baseUrl: "http://x", fetch: f });
    await c.getSandbox("sb id with space");
    expect(calls[0]!.url).toBe("http://x/v1/sandboxes/sb%20id%20with%20space");
  });

  it("aborts after timeout", async () => {
    // Realistic abort path: fetch that respects AbortSignal. The
    // Controller schedules a setTimeout(timeoutMs) that calls
    // controller.abort(); a well-behaved fetch rejects with AbortError.
    const abortableFetch: typeof fetch = ((_url, init) =>
      new Promise((_, reject) => {
        const sig = (init as RequestInit | undefined)?.signal;
        if (!sig) return;
        sig.addEventListener("abort", () => {
          const err = new Error("aborted");
          err.name = "AbortError";
          reject(err);
        });
      })) as typeof fetch;
    const c = new Controller({ timeoutMs: 10, fetch: abortableFetch });
    await expect(c.listSnapshots()).rejects.toThrow(/abort/i);
  });
});
