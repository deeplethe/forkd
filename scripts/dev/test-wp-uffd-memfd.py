#!/usr/bin/env python3
"""Phase 6.1.5 smoke test against memfd-backed guest memory.

Reproduces what forkd-controller will do at branch time:
  1. memfd_create + ftruncate + populate from memory.bin
  2. boot FC, restore snapshot with backend_path = /proc/$$/fd/<memfd>
  3. PUT /uffd/wp — FC creates uffd, registers WP on its memfd VMA,
     sends fd via SCM_RIGHTS to our listener
  4. verify the received fd is a valid uffd

Sequence kept in-process so the memfd stays alive for FC's lifetime
(closing the fd would invalidate FC's mmap).
"""
import ctypes
import ctypes.util
import json
import os
import socket
import struct
import subprocess
import sys
import time
import urllib.request

FC_BIN = "/home/yangdongxu/firecracker-fork/build/cargo_target/x86_64-unknown-linux-musl/release/firecracker"
SNAP_DIR = os.path.expanduser("~/.local/share/forkd/snapshots/coding-agent-fork-prewarm-v1")
MEMORY_BIN = os.path.join(SNAP_DIR, "memory.bin")
VMSTATE = os.path.join(SNAP_DIR, "vmstate")

libc = ctypes.CDLL(ctypes.util.find_library("c"), use_errno=True)

SYS_memfd_create = 319
MFD_CLOEXEC = 0x0001


def memfd_create(name: bytes, flags: int = MFD_CLOEXEC) -> int:
    fd = libc.syscall(SYS_memfd_create, name, flags)
    if fd < 0:
        raise OSError(ctypes.get_errno(), os.strerror(ctypes.get_errno()))
    return fd


def http_unix(sock_path: str, method: str, path: str, body: dict | None = None) -> tuple[int, str]:
    """Talk HTTP to a unix-domain socket. Returns (status, body)."""
    body_bytes = json.dumps(body).encode() if body else b""
    req = f"{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {len(body_bytes)}\r\n\r\n".encode() + body_bytes
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.connect(sock_path)
    s.sendall(req)
    raw = b""
    while True:
        chunk = s.recv(4096)
        if not chunk:
            break
        raw += chunk
        if b"\r\n\r\n" in raw:
            # Got headers; if it's 204, body is empty
            head, _, rest = raw.partition(b"\r\n\r\n")
            status = int(head.split(b" ", 2)[1])
            if status == 204:
                s.close()
                return status, ""
            # Otherwise wait for body — assume small JSON
            # crude: read until close or content-length
            if b"content-length:" in head.lower():
                cl = int(head.lower().split(b"content-length:")[1].split(b"\r\n")[0].strip())
                while len(rest) < cl:
                    chunk = s.recv(4096)
                    if not chunk:
                        break
                    rest += chunk
                s.close()
                return status, rest[:cl].decode("utf-8", errors="replace")
            s.close()
            return status, rest.decode("utf-8", errors="replace")
    s.close()
    return 0, raw.decode("utf-8", errors="replace")


def main():
    workdir = f"/tmp/fc-wp-memfd-{os.getpid()}"
    os.makedirs(workdir, exist_ok=True)
    fc_sock = os.path.join(workdir, "api.sock")
    wp_sock = os.path.join(workdir, "wp.sock")
    fc_log = os.path.join(workdir, "fc.log")
    fc_stderr = os.path.join(workdir, "fc.stderr")
    os.system(f"sudo touch {fc_log}")

    # 1. Create memfd + populate from memory.bin
    print(f"[1] memfd_create + populate from {MEMORY_BIN}")
    src_size = os.path.getsize(MEMORY_BIN)
    print(f"    memory.bin size = {src_size} bytes ({src_size // (1024*1024)} MiB)")
    mfd = memfd_create(b"forkd-wp-test", MFD_CLOEXEC)
    if libc.ftruncate(mfd, src_size) < 0:
        raise OSError(ctypes.get_errno(), "ftruncate")
    with open(MEMORY_BIN, "rb") as src:
        bytes_copied = 0
        while bytes_copied < src_size:
            chunk = src.read(4 * 1024 * 1024)
            if not chunk:
                break
            written = os.write(mfd, chunk)
            bytes_copied += written
    print(f"    populated memfd fd={mfd} with {bytes_copied} bytes")
    memfd_path = f"/proc/{os.getpid()}/fd/{mfd}"
    print(f"    FC will see it as {memfd_path}")

    # 2. Launch FC
    print("[2] launch FC")
    fc_proc = subprocess.Popen(
        ["sudo", FC_BIN, "--no-seccomp", "--api-sock", fc_sock, "--log-path", fc_log, "--level", "Debug"],
        stdout=subprocess.DEVNULL,
        stderr=open(fc_stderr, "wb"),
        pass_fds=(mfd,),  # so the FC sudo child can inherit the memfd
    )
    time.sleep(0.5)
    # Find the real FC pid (sudo's child)
    fc_pid = int(subprocess.check_output(["pgrep", "-P", str(fc_proc.pid)]).decode().strip().split("\n")[0])
    print(f"    FC pid = {fc_pid}, sudo wrapper = {fc_proc.pid}")

    # 3. Restore snapshot with backend pointing at our memfd
    print("[3] /snapshot/load (backend=memfd via /proc)")
    status, body = http_unix(fc_sock, "PUT", "/snapshot/load", {
        "snapshot_path": VMSTATE,
        "mem_backend": {
            "backend_path": memfd_path,
            "backend_type": "File",
            "shared": True,
        },
        "enable_diff_snapshots": False,
        "resume_vm": True,
    })
    print(f"    -> {status} {body[:200] if body else ''}")
    if status != 204:
        print("    LOAD FAILED")
        sys.exit(1)

    # 4. Verify FC actually mmap'd a memfd-backed VMA
    print("[4] check FC /proc/{}/maps for memfd-backed VMA".format(fc_pid))
    maps = subprocess.check_output(["sudo", "cat", f"/proc/{fc_pid}/maps"]).decode()
    for line in maps.splitlines():
        if "memfd" in line.lower() or "forkd-wp" in line:
            print(f"    {line}")
        if "memory.bin" in line:
            print(f"    LEGACY: {line}")  # should NOT appear

    # 5. Start UDS receiver
    print("[5] start SCM_RIGHTS receiver on", wp_sock)
    try:
        os.unlink(wp_sock)
    except FileNotFoundError:
        pass
    srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    srv.bind(wp_sock)
    os.chmod(wp_sock, 0o666)
    srv.listen(1)

    # 6. PUT /uffd/wp
    print("[6] PUT /uffd/wp")
    import threading
    received = []
    def receiver():
        conn, _ = srv.accept()
        msg, ancdata, _flags, _addr = conn.recvmsg(8192, socket.CMSG_LEN(struct.calcsize("i")))
        for level, ctype, cdata in ancdata:
            if level == socket.SOL_SOCKET and ctype == socket.SCM_RIGHTS:
                n = len(cdata) // struct.calcsize("i")
                fds = struct.unpack(f"{n}i", cdata[: n * struct.calcsize("i")])
                received.append((msg, fds))
        conn.close()
    t = threading.Thread(target=receiver, daemon=True)
    t.start()
    time.sleep(0.2)

    status, body = http_unix(fc_sock, "PUT", "/uffd/wp", {"socket": wp_sock})
    print(f"    -> {status} {body[:300] if body else ''}")
    t.join(timeout=2)

    if received:
        msg, fds = received[0]
        print(f"[7] receiver got: payload={msg[:200]!r}, fds={fds}")
        # Check if fd is usable as uffd
        for fd in fds:
            try:
                link = os.readlink(f"/proc/self/fd/{fd}")
                print(f"    fd {fd} -> {link}")
            except OSError as e:
                print(f"    fd {fd} readlink failed: {e}")
    else:
        print("[7] no fd received via SCM_RIGHTS")

    print("[8] FC stderr (eprintln):")
    with open(fc_stderr, "rb") as f:
        for line in f.read().decode("utf-8", errors="replace").splitlines():
            if "forkd-wp" in line:
                print(f"    {line}")

    # Cleanup
    subprocess.run(["sudo", "kill", "-9", str(fc_pid), str(fc_proc.pid)], stderr=subprocess.DEVNULL)
    fc_proc.wait(timeout=5)
    os.close(mfd)


if __name__ == "__main__":
    main()
