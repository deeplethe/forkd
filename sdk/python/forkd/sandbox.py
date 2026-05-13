"""E2B-compatible Sandbox wrapper around forkd's guest agent."""

from __future__ import annotations

import json
import os
import shutil
import socket
import subprocess
import time
from dataclasses import dataclass
from typing import Optional, Sequence, Union


@dataclass
class CommandResult:
    """Result of `sandbox.commands.run(...)`. Mirrors E2B's API."""

    stdout: str
    stderr: str
    exit_code: int


class _CommandsProxy:
    """Implements the `sandbox.commands` namespace from E2B's API."""

    def __init__(self, sandbox: "Sandbox") -> None:
        self._sandbox = sandbox

    def run(
        self,
        cmd: Union[str, Sequence[str]],
        timeout: int = 30,
    ) -> CommandResult:
        """Run a command inside the sandbox and return its output.

        `cmd` can be a string (executed via `sh -c`) or a list/tuple of
        argv tokens (executed directly).
        """
        if isinstance(cmd, str):
            args = ["/bin/sh", "-c", cmd]
        else:
            args = list(cmd)
        resp = self._sandbox._send({"action": "exec", "args": args, "timeout": timeout})
        if "error" in resp and "stdout" not in resp:
            return CommandResult(stdout="", stderr=resp["error"], exit_code=1)
        return CommandResult(
            stdout=resp.get("stdout", ""),
            stderr=resp.get("stderr", ""),
            exit_code=int(resp.get("exit_code", -1)),
        )


class Sandbox:
    """Open one forked microVM sandbox, E2B-compatible surface.

    Example
    -------
    >>> with Sandbox() as sb:
    ...     print(sb.commands.run("echo hi").stdout)
    hi
    """

    DEFAULT_TAG = os.environ.get("FORKD_TAG", "pyagent")
    DEFAULT_TARGET = os.environ.get("FORKD_TARGET", "10.42.0.2:8888")

    def __init__(
        self,
        tag: Optional[str] = None,
        target: Optional[str] = None,
        timeout: int = 30,
        *,
        spawn: bool = True,
    ) -> None:
        self.tag = tag or self.DEFAULT_TAG
        self.target = target or self.DEFAULT_TARGET
        self.timeout = timeout
        self.commands = _CommandsProxy(self)
        self._fork_proc: Optional[subprocess.Popen] = None
        if spawn:
            self._spawn()

    # ----- public API -----------------------------------------------------

    def eval(self, code: str) -> object:
        """Evaluate code against the warmed PID-1 process.

        Not part of E2B's API. forkd's killer move: the parent VM's
        runtime is already warm, so `eval` returns in single-digit ms
        instead of ~100 ms for a fresh `python3 -c "..."` subprocess.

        Semantics depend on the recipe:

        - **Python recipes** (default): `code` is a Python expression
          evaluated against the agent's interpreter. `numpy` is in
          scope when the image has it installed. Returns the
          ``repr()`` of the evaluated value as a string.

        - **Node recipes** (recipe sets ``FORKD_AGENT_LANG=node`` in
          ``/etc/forkd-recipe.env``, e.g. ``playwright-browser``):
          `code` is an async-function body run with
          ``(browser, context, page)`` in scope (recipe-specific).
          Returns the JSON-decoded result as a native Python object.
          Top-level ``await`` is supported; use ``return`` to send a
          value back.

        Examples
        --------
        >>> # Python recipe
        >>> sb.eval("numpy.zeros(5).tolist()")
        '[0.0, 0.0, 0.0, 0.0, 0.0]'

        >>> # playwright-browser recipe
        >>> sb.eval("await page.goto('https://example.com'); "
        ...         "return await page.title()")
        'Example Domain'
        """
        resp = self._send({"action": "eval", "code": code})
        if "error" in resp:
            raise RuntimeError(f"forkd eval: {resp['error']}")
        # Node recipes return a JSON-encoded result; deserialise so the
        # caller gets a native Python value. Python recipes still return
        # the repr() string (unchanged backwards-compat behaviour).
        if "result_json" in resp:
            return json.loads(resp["result_json"])
        return resp.get("result")

    def ping(self) -> dict:
        """Probe the guest agent. Returns dict with 'pong' and 'numpy_version'."""
        return self._send({"action": "ping"})

    @classmethod
    def create(cls, *args, **kwargs) -> "Sandbox":
        """Alias for `Sandbox(...)` matching E2B's `Sandbox.create()` style."""
        return cls(*args, **kwargs)

    def kill(self) -> None:
        """Terminate the underlying forked microVM."""
        if self._fork_proc is None:
            return
        self._fork_proc.terminate()
        try:
            self._fork_proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self._fork_proc.kill()
        self._fork_proc = None

    # ----- context manager ------------------------------------------------

    def __enter__(self) -> "Sandbox":
        return self

    def __exit__(self, *exc) -> None:
        self.kill()

    # ----- internals ------------------------------------------------------

    def _spawn(self) -> None:
        if shutil.which("forkd") is None:
            raise RuntimeError(
                "the `forkd` Rust CLI must be on PATH. "
                "Build it with `cargo build --release -p forkd-cli` and add "
                "target/release to PATH."
            )

        # Capture stderr so we can show why a fork failed.
        self._fork_proc = subprocess.Popen(
            [
                "forkd",
                "fork",
                "--tag",
                self.tag,
                "-n",
                "1",
                "--settle-secs",
                "3600",
            ],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
        )

        # Poll the agent until it responds (~150 ms typical).
        deadline = time.time() + 30
        last_err: Optional[Exception] = None
        while time.time() < deadline:
            try:
                self.ping()
                return
            except (OSError, socket.error) as e:
                last_err = e
                # If forkd itself died, bubble its stderr up — that's the
                # actually useful information.
                rc = self._fork_proc.poll() if self._fork_proc else None
                if rc is not None and rc != 0:
                    err_bytes = self._fork_proc.stderr.read() if self._fork_proc.stderr else b""
                    raise RuntimeError(
                        f"forkd fork --tag {self.tag} exited with code {rc}:\n"
                        f"{err_bytes.decode(errors='replace')}"
                    )
                time.sleep(0.1)
        self.kill()
        raise RuntimeError(
            f"sandbox didn't come up at {self.target} within 30s "
            f"(last error: {last_err})"
        )

    def _send(self, msg: dict) -> dict:
        host, _, port_s = self.target.rpartition(":")
        port = int(port_s)
        with socket.create_connection((host, port), timeout=5) as s:
            s.settimeout(self.timeout + 5)
            s.sendall((json.dumps(msg) + "\n").encode())
            s.shutdown(socket.SHUT_WR)
            buf = bytearray()
            while True:
                chunk = s.recv(65536)
                if not chunk:
                    break
                buf.extend(chunk)
        return json.loads(buf.decode())
