"""HTTP client for the forkd-controller daemon's REST API.

The `Controller` class wraps `/v1/snapshots` and `/v1/sandboxes` endpoints
(docs/API.md) so Python agent code can manage snapshots, fork sandboxes,
branch running sandboxes, and tear things down without shelling out to
`forkd`.

Talking to the controller is orthogonal to the in-guest `Sandbox` agent
class (`forkd.Sandbox`): Controller manages VM lifecycle from the host
side; Sandbox talks to the in-guest agent on TCP for exec/eval inside
one specific child VM. Most agent runtimes use both — Controller to
spawn/branch/kill, Sandbox to drive code execution.

Example
-------

>>> from forkd import Controller, Sandbox
>>> c = Controller()  # default http://127.0.0.1:8889, no token
>>> [s["tag"] for s in c.list_snapshots()]
['pyagent']
>>> children = c.spawn_sandboxes("pyagent", n=1, per_child_netns=True)
>>> sb_id = children[0]["id"]
>>> # ... drive the sandbox via Sandbox(target=children[0]['guest_addr'])
>>> branch = c.branch_sandbox(sb_id, tag="checkpoint-1")
>>> branch["tag"]
'checkpoint-1'
>>> branch["branched_from"]
'sb-...'
>>> grandchildren = c.spawn_sandboxes(branch["tag"], n=5)
>>> c.kill_sandbox(sb_id)
"""

from __future__ import annotations

import json
import os
import urllib.error
import urllib.request
from typing import Any, Literal, Optional

BranchMode = Literal["full", "diff", "live"]
"""Canonical BRANCH mode selector (Phase 7.1+).

- ``"full"`` — copy entire guest RAM under pause (default for v0.x).
- ``"diff"`` — Firecracker Diff snapshot (v0.3+). Sub-second pause for
  idle sources; replaces the legacy ``diff=True`` boolean.
- ``"live"`` — UFFD_WP-based live BRANCH (v0.4+). Source pause drops to
  sub-50 ms; memory streams from the running parent. Requires the
  source to have been spawned with ``live_fork=True``.
"""


class ControllerError(RuntimeError):
    """Raised on non-2xx responses from the daemon.

    Carries the HTTP status code and the daemon's parsed error body
    (when it returned JSON). Inspect ``status`` and ``body`` to
    distinguish 404 (sandbox/snapshot missing) from 409 (tag
    collision) from 500 (internal).
    """

    def __init__(self, status: int, body: Any, url: str) -> None:
        self.status = status
        self.body = body
        self.url = url
        snippet = body if isinstance(body, str) else json.dumps(body)
        super().__init__(f"controller {url}: HTTP {status}: {snippet}")


class Controller:
    """Client for the forkd-controller daemon's REST API.

    Parameters
    ----------
    base_url:
        Daemon base URL. Defaults to ``$FORKD_URL`` then
        ``http://127.0.0.1:8889``.
    token:
        Bearer token. Defaults to ``$FORKD_TOKEN``. Required only when
        the daemon was started with ``--token-file``.
    timeout:
        Per-request timeout in seconds. Branching can take 0.5-8 s on
        a large parent VM; default is generous.
    """

    def __init__(
        self,
        base_url: Optional[str] = None,
        token: Optional[str] = None,
        timeout: float = 60.0,
    ) -> None:
        self.base_url = (
            base_url
            or os.environ.get("FORKD_URL")
            or "http://127.0.0.1:8889"
        ).rstrip("/")
        self.token = token if token is not None else os.environ.get("FORKD_TOKEN")
        self.timeout = timeout

    # --- snapshots ------------------------------------------------

    def list_snapshots(self) -> list[dict]:
        """``GET /v1/snapshots`` — every snapshot known to the daemon."""
        return self._request("GET", "/v1/snapshots")

    def delete_snapshot(self, tag: str) -> None:
        """``DELETE /v1/snapshots/:tag`` — drop both registry and disk files."""
        self._request("DELETE", f"/v1/snapshots/{tag}")

    # --- sandboxes ------------------------------------------------

    def spawn_sandboxes(
        self,
        snapshot_tag: str,
        n: int = 1,
        per_child_netns: bool = False,
        memory_limit_mib: Optional[int] = None,
        prewarm: bool = False,
        live_fork: bool = False,
        hugepages: bool = False,
    ) -> list[dict]:
        """``POST /v1/sandboxes`` — fork N children from a snapshot tag.

        Parameters
        ----------
        prewarm:
            When true, each child performs a throwaway snapshot to
            scratch storage immediately after restore to fault-in all
            guest pages. Trades ~170 ms / 512 MiB of extra spawn time
            for steady-state BRANCH latency on the first user-visible
            BRANCH (avoids the 2-9× cold-cache penalty documented in
            ``bench/pause-window/RESULTS-v0.2.md``).
        live_fork:
            v0.4+. Boot the sandbox with a memfd-backed RAM region so
            later BRANCHes from it can use ``mode="live"`` (UFFD_WP).
            Requires kernel 5.7+ and the vendored Firecracker fork —
            see ``docs/VENDORED-FIRECRACKER.md``. No effect at spawn
            time beyond the backend swap; cost shows up on the first
            live BRANCH.
        hugepages:
            v0.4+. Back the memfd with 2 MiB hugepages
            (``MFD_HUGETLB | MFD_HUGE_2MB``). Only meaningful with
            ``live_fork=True``. Reduces TLB pressure during spawn-many
            and live BRANCH bulk-copy. Requires non-zero
            ``HugePages_Free`` in ``/proc/meminfo`` — ``forkd doctor``
            checks availability. Falls back to normal 4 KiB pages with
            a warning if the pool is exhausted.

        Returns the list of SandboxInfo dicts (id, snapshot_tag, netns,
        guest_addr, created_at_unix, pid, memory_limit_mib).
        """
        body: dict[str, Any] = {
            "snapshot_tag": snapshot_tag,
            "n": n,
            "per_child_netns": per_child_netns,
        }
        if memory_limit_mib is not None:
            body["memory_limit_mib"] = memory_limit_mib
        if prewarm:
            body["prewarm"] = True
        if live_fork:
            body["live_fork"] = True
        if hugepages:
            body["hugepages"] = True
        return self._request("POST", "/v1/sandboxes", body)

    def list_sandboxes(self) -> list[dict]:
        """``GET /v1/sandboxes`` — every live sandbox the daemon tracks."""
        return self._request("GET", "/v1/sandboxes")

    def get_sandbox(self, sandbox_id: str) -> dict:
        """``GET /v1/sandboxes/:id`` — one sandbox's metadata."""
        return self._request("GET", f"/v1/sandboxes/{sandbox_id}")

    def kill_sandbox(self, sandbox_id: str) -> None:
        """``DELETE /v1/sandboxes/:id`` — terminate one sandbox."""
        self._request("DELETE", f"/v1/sandboxes/{sandbox_id}")

    def branch_sandbox(
        self,
        sandbox_id: str,
        tag: Optional[str] = None,
        diff: bool = False,
        measure_diff: bool = False,
        mode: Optional[BranchMode] = None,
        wait: bool = True,
    ) -> dict:
        """``POST /v1/sandboxes/:id/branch`` — pause + snapshot + resume.

        Parameters
        ----------
        mode:
            v0.4+ canonical selector. ``"full"``, ``"diff"``, or
            ``"live"``. When set, takes precedence over the legacy
            ``diff`` boolean — and passing both raises
            :class:`ControllerError` (HTTP 400). Prefer this over
            ``diff=`` in new code. See :data:`BranchMode`.
        diff:
            **Legacy.** Equivalent to ``mode="diff"``; kept so this SDK
            can drive v0.3.x daemons that don't understand ``mode``.
            Mutually exclusive with ``mode`` (server-side).
        measure_diff:
            v0.3+: measurement-only hook. Take a Diff snapshot inside
            the existing Full pause to report what diff would have
            cost, without changing semantics. Mutually exclusive with
            ``diff`` (daemon returns 400 if both are true).
        wait:
            v0.4+, only meaningful with ``mode="live"``. Default
            ``True`` blocks until the background memory copy finishes
            and the returned snapshot is ``status="ready"``. Set to
            ``False`` to return as soon as the source resumes (~10 ms);
            the snapshot reaches ``status="ready"`` later — poll
            :meth:`list_snapshots` to detect completion.

        The source sandbox is paused for the duration of the snapshot
        write — typically 0.5-8 s for Full, ~200 ms for Diff, sub-50 ms
        for Live — then resumed. The returned snapshot is independent
        of the source's lifecycle.

        Returns a SnapshotInfo dict; pass its ``tag`` to
        ``spawn_sandboxes`` to fork grandchildren from the branch.
        """
        body: dict[str, Any] = {}
        if tag is not None:
            body["tag"] = tag
        # Prefer canonical `mode` when set; fall back to legacy `diff`
        # so older daemons keep working unchanged.
        if mode is not None:
            body["mode"] = mode
        elif diff:
            body["diff"] = True
        if measure_diff:
            body["measure_diff"] = True
        # `wait=True` is the daemon default; only send when the caller
        # opted into fire-and-forget so the body stays minimal against
        # daemons that don't recognize the field.
        if not wait:
            body["wait"] = False
        return self._request("POST", f"/v1/sandboxes/{sandbox_id}/branch", body)

    def exec_command(
        self,
        sandbox_id: str,
        args: list[str],
        timeout_secs: int = 30,
    ) -> dict:
        """``POST /v1/sandboxes/:id/exec`` — run a subprocess in the sandbox.

        Returns ``{stdout, stderr, exit_code}``.
        """
        return self._request(
            "POST",
            f"/v1/sandboxes/{sandbox_id}/exec",
            {"args": args, "timeout_secs": timeout_secs},
        )

    def eval_code(self, sandbox_id: str, code: str) -> dict:
        """``POST /v1/sandboxes/:id/eval`` — eval against warmed PID-1.

        Returns ``{result, error, exit_code}``.
        """
        return self._request(
            "POST",
            f"/v1/sandboxes/{sandbox_id}/eval",
            {"code": code},
        )

    def ping_sandbox(self, sandbox_id: str) -> dict:
        """``POST /v1/sandboxes/:id/ping`` — round-trip to the guest agent."""
        return self._request("POST", f"/v1/sandboxes/{sandbox_id}/ping")

    # --- internals ------------------------------------------------

    def _request(self, method: str, path: str, body: Optional[dict] = None) -> Any:
        url = f"{self.base_url}{path}"
        data = json.dumps(body).encode() if body is not None else None
        headers = {"Content-Type": "application/json"} if body is not None else {}
        if self.token:
            headers["Authorization"] = f"Bearer {self.token}"
        req = urllib.request.Request(url, data=data, method=method, headers=headers)
        try:
            with urllib.request.urlopen(req, timeout=self.timeout) as resp:
                raw = resp.read()
                if not raw:
                    return None
                return json.loads(raw)
        except urllib.error.HTTPError as e:
            raw = e.read()
            parsed: Any
            try:
                parsed = json.loads(raw) if raw else {}
            except json.JSONDecodeError:
                parsed = raw.decode(errors="replace")
            raise ControllerError(e.code, parsed, url) from e
