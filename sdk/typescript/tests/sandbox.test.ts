import { describe, expect, it } from "vitest";
import { Controller, SANDBOX_TOKEN_HEADER } from "../src/controller.js";
import { Sandbox } from "../src/sandbox.js";
import type { SandboxInfo } from "../src/types.js";

function info(id: string, token?: string): SandboxInfo {
  return { id, token, snapshot_tag: "py", netns: null, guest_addr: "", created_at_unix: 1, pid: null, memory_limit_mib: null };
}

function server(requireToken = true) {
  const calls: Request[] = [];
  const fetchMock: typeof fetch = async (input, init) => {
    const request = new Request(input, init);
    calls.push(request);
    expect(request.headers.get("authorization")).toBe("Bearer api-secret");
    const path = new URL(request.url).pathname;
    if (path === "/v1/sandboxes") {
      expect(request.headers.has(SANDBOX_TOKEN_HEADER)).toBe(false);
      return Response.json([info("a", "token-a"), info("b", "token-b")]);
    }
    const [, , , id, operation] = path.split("/");
    if (requireToken && request.headers.get(SANDBOX_TOKEN_HEADER) !== `token-${id}`) {
      return Response.json({ error: "sandbox access denied" }, { status: 403 });
    }
    if (operation === "exec") return Response.json({ stdout: "ok", stderr: "", exit_code: 0 });
    if (operation === "eval") {
      const body = await request.clone().json();
      return Response.json({ result: 4, error: body.code === "fail" ? "evaluation failed" : null, exit_code: 0 });
    }
    return Response.json({ healthy: true });
  };
  return { calls, options: { baseUrl: "http://test", token: "api-secret", fetch: fetchMock } };
}

describe("Sandbox credentials", () => {
  it("forwards creation tokens through all high-level operations", async () => {
    const { calls, options } = server();
    const sandbox = await Sandbox.create({ ...options, snapshotTag: "py" });
    expect(await sandbox.exec(["echo", "ok"], { timeoutSecs: 17 })).toEqual({ stdout: "ok", stderr: "", exit_code: 0 });
    expect(await sandbox.eval("2+2")).toBe(4);
    expect(await sandbox.ping()).toEqual({ healthy: true });
    expect(await calls[1]!.clone().json()).toEqual({ args: ["echo", "ok"], timeout_secs: 17 });
    for (const request of calls.slice(1)) expect(request.headers.get(SANDBOX_TOKEN_HEADER)).toBe("token-a");
    await expect(sandbox.eval("fail")).rejects.toThrow("eval error: evaluation failed");
  });

  it("keeps tokens separate when wrapping creation results and interleaving calls", async () => {
    const { calls, options } = server();
    const controller = new Controller(options);
    const created = await controller.spawnSandboxes({ snapshotTag: "py", n: 2 });
    const a = new Sandbox(controller, created[0]!);
    const b = new Sandbox(controller, created[1]!);
    await a.exec(["true"]);
    await b.eval("2+2");
    await a.ping();
    await b.exec(["true"]);
    await a.eval("2+2");
    await b.ping();
    expect(calls.slice(1).map(request => request.headers.get(SANDBOX_TOKEN_HEADER))).toEqual(["token-a", "token-b", "token-a", "token-b", "token-a", "token-b"]);
    expect(await calls[1]!.clone().json()).toEqual({ args: ["true"], timeout_secs: 30 });
  });

  it.each([undefined, "wrong"])("preserves server failures with token %s", async token => {
    const { options } = server();
    const sandbox = new Sandbox(new Controller(options), info("a", token));
    for (const operation of [() => sandbox.exec(["true"]), () => sandbox.eval("2+2"), () => sandbox.ping()]) {
      await expect(operation()).rejects.toMatchObject({ name: "ControllerError", status: 403, body: { error: "sandbox access denied" } });
    }
  });

  it("omits the sandbox header for older servers without substituting the Bearer", async () => {
    const { calls, options } = server(false);
    const sandbox = new Sandbox(new Controller(options), info("a"));
    await sandbox.exec(["true"]);
    expect(await sandbox.eval("2+2")).toBe(4);
    expect(await sandbox.ping()).toEqual({ healthy: true });
    for (const request of calls) expect(request.headers.has(SANDBOX_TOKEN_HEADER)).toBe(false);
  });
});
