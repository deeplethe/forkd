"""
Quick empirical test: does MAP_PRIVATE cut off the v0.4 path?

Models the FC scenario:
  Process A (forkd) holds memfd open + mmap MAP_SHARED.
  Process B (FC) opens the same memfd via /proc/A_pid/fd/N + mmap MAP_PRIVATE
  (matching what FC does for File backend).

Test:
  1. A writes pattern X to its MAP_SHARED region.
  2. B reads its MAP_PRIVATE region → should see X (pre-CoW read).
  3. B writes pattern Y to its MAP_PRIVATE region.
  4. A reads → should still see X (B's writes are private, don't propagate).
  5. A writes pattern Z.
  6. B reads → should see Y where B wrote, X-or-Z elsewhere (CoW already
     broke for the pages B touched).

If step 4 shows A still sees X (not Y), MAP_PRIVATE is confirmed dead
for the v0.4 path.
"""

import os
import sys
import ctypes
import mmap
import time

libc = ctypes.CDLL("libc.so.6")
libc.memfd_create.restype = ctypes.c_int
libc.memfd_create.argtypes = [ctypes.c_char_p, ctypes.c_uint]


def main():
    parent_pid = os.getpid()
    SIZE = 4096

    fd = libc.memfd_create(b"v0.4-map-private-test", 0)
    if fd < 0:
        print("memfd_create failed")
        sys.exit(1)
    os.ftruncate(fd, SIZE)

    # Process A: MAP_SHARED on the memfd (the v0.4 forkd-side).
    a_map = mmap.mmap(fd, SIZE, mmap.MAP_SHARED, mmap.PROT_READ | mmap.PROT_WRITE)

    # 1. A writes pattern X.
    a_map[:6] = b"XXXXXX"
    a_map.flush()
    print(f"[A] wrote 'XXXXXX' to MAP_SHARED region")

    # 2. Spawn child as "process B" (the FC-side).
    r, w = os.pipe()  # for child→parent ack
    pid = os.fork()
    if pid == 0:
        os.close(r)
        # Child opens memfd via /proc/<parent>/fd/<N>, mmap MAP_PRIVATE
        # — this is what FC does for `File` backend.
        cfd = os.open(f"/proc/{parent_pid}/fd/{fd}", os.O_RDWR)
        b_map = mmap.mmap(
            cfd, SIZE, mmap.MAP_PRIVATE, mmap.PROT_READ | mmap.PROT_WRITE
        )

        # 2a. B reads — should see X (the populated content).
        seen_before_write = bytes(b_map[:6])
        print(f"[B] pre-write read: {seen_before_write!r}")

        # 3. B writes Y. This triggers CoW; B now has its own private page.
        b_map[:6] = b"YYYYYY"
        b_map.flush()
        print(f"[B] wrote 'YYYYYY' (MAP_PRIVATE, expect CoW break)")

        # Signal parent to do step 5
        os.write(w, b"x")
        os.close(w)
        # Wait for parent to write Z
        time.sleep(0.5)

        # 6. B reads — should still see Y (post-CoW, B's own copy).
        seen_after_parent = bytes(b_map[:6])
        print(f"[B] post-A-write read: {seen_after_parent!r}")

        b_map.close()
        os.close(cfd)
        os._exit(0)

    # Parent waits for B's signal.
    os.close(w)
    os.read(r, 1)
    os.close(r)

    # 4. A reads — should still see X (B's write didn't propagate).
    after_b_write = bytes(a_map[:6])
    print(f"[A] post-B-write read: {after_b_write!r}")
    if after_b_write == b"YYYYYY":
        print("[A] !!! B's MAP_PRIVATE write propagated to A. This shouldn't happen.")
    elif after_b_write == b"XXXXXX":
        print("[A] ✓ B's MAP_PRIVATE write did NOT propagate to A (expected).")
        print("    Confirms: MAP_PRIVATE on FC side breaks v0.4's guest→forkd path.")
    else:
        print(f"[A] unexpected content: {after_b_write!r}")

    # 5. A writes Z to test the OTHER direction (A→B after B's CoW).
    a_map[:6] = b"ZZZZZZ"
    a_map.flush()
    print(f"[A] wrote 'ZZZZZZ' to MAP_SHARED region")

    # Wait for child to finish its post-write read.
    os.waitpid(pid, 0)

    a_map.close()
    os.close(fd)

    print("\n=== Conclusion ===")
    print("If [A] read showed XXXXXX (not YYYYYY) after [B]'s write,")
    print("then MAP_PRIVATE on FC's side cuts the guest→forkd write path.")
    print("v0.4 cannot use /proc/<forkd_pid>/fd/N + File backend without")
    print("an FC patch to use MAP_SHARED.")


if __name__ == "__main__":
    main()
