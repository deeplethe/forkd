"""Regression tests for Sandbox._send / target parsing (issue #256).

These cover the two bugs in the original `_send`:
  1. `rpartition(":")` mangled IPv6 targets — fixed via `_split_host_port`.
  2. an empty response body raised a baffling `JSONDecodeError` instead
     of a clear "agent closed the connection" error.

No live guest agent needed: `_split_host_port` is a pure function, and
`_send` is exercised against a tiny in-process TCP server. `Sandbox` is
constructed with `spawn=False` so nothing forks.
"""
import json
import socket
import threading

import pytest

from forkd.sandbox import Sandbox, _split_host_port


# ---- _split_host_port (bug 1: IPv6) ----------------------------------------

def test_split_ipv4():
    assert _split_host_port("10.42.0.2:8888") == ("10.42.0.2", 8888)


def test_split_hostname():
    assert _split_host_port("agent.local:9000") == ("agent.local", 9000)


def test_split_ipv6_bracketed():
    # The original rpartition(":") left brackets in the host; urlsplit
    # strips them so the bare address reaches create_connection.
    assert _split_host_port("[::1]:8888") == ("::1", 8888)


def test_split_ipv6_full():
    assert _split_host_port("[2001:db8::1]:443") == ("2001:db8::1", 443)


@pytest.mark.parametrize("bad", ["nohost", "10.42.0.2", "host:", ":8888", "host:notaport"])
def test_split_malformed_raises_valueerror(bad):
    with pytest.raises(ValueError):
        _split_host_port(bad)


# ---- _send against a local server ------------------------------------------

def _serve_once(reply: bytes):
    """Start a one-shot TCP server on 127.0.0.1; return its 'host:port'.
    Each connection gets `reply` then the socket closes."""
    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.bind(("127.0.0.1", 0))
    srv.listen(1)
    host, port = srv.getsockname()

    def handle():
        conn, _ = srv.accept()
        with conn:
            # Drain the request (the client SHUT_WR after sending).
            while conn.recv(65536):
                pass
            if reply:
                conn.sendall(reply)
        srv.close()

    threading.Thread(target=handle, daemon=True).start()
    return f"{host}:{port}"


def _sandbox_for(target: str) -> Sandbox:
    return Sandbox(target=target, timeout=2, spawn=False)


def test_send_roundtrip_ok():
    target = _serve_once((json.dumps({"ok": True, "echo": 1}) + "\n").encode())
    sb = _sandbox_for(target)
    assert sb._send({"ping": 1}) == {"ok": True, "echo": 1}


def test_send_empty_response_raises_clear_error():
    # bug 2: server closes without replying. The RuntimeError("empty
    # response") proves we no longer surface a baffling JSONDecodeError
    # from json.loads("").
    target = _serve_once(b"")
    sb = _sandbox_for(target)
    with pytest.raises(RuntimeError, match="empty response"):
        sb._send({"ping": 1})


def test_send_invalid_target_raises_before_connect():
    sb = _sandbox_for("not-a-target")
    with pytest.raises(ValueError, match="invalid forkd target"):
        sb._send({"ping": 1})
