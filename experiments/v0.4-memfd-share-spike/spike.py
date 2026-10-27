"""
30-second spike: can a memfd be opened cross-process via /proc/<pid>/fd/<N>?
This is the key question for v0.4: if sandstorm creates a memfd, can FC
open it via the procfs path sandstorm hands it (`mem_backend.backend_path
= /proc/sandstorm-pid/fd/N`)?

Stage 1: same-process re-open (sanity check — should always work).
Stage 2: child process opens parent's memfd by path.
Stage 3: writes through child's fd are visible in parent's mmap.
"""

import os
import sys
import ctypes
import time

libc = ctypes.CDLL("libc.so.6")
libc.memfd_create.restype = ctypes.c_int
libc.memfd_create.argtypes = [ctypes.c_char_p, ctypes.c_uint]


def main():
    fd = libc.memfd_create(b"sandstorm-memfd-spike", 0)
    if fd < 0:
        print("memfd_create failed")
        sys.exit(1)
    print(f"[parent] created memfd fd={fd}, pid={os.getpid()}")

    SIZE = 4096
    os.ftruncate(fd, SIZE)
    os.write(fd, b"BEFORE_PARENT_WROTE" + b"_" * (SIZE - 20))

    # Stage 1: same-process re-open via /proc/self/fd
    fd2 = os.open(f"/proc/{os.getpid()}/fd/{fd}", os.O_RDWR)
    print(f"[stage 1] same-process re-open: fd2={fd2}")
    os.lseek(fd2, 0, 0)
    print(f"[stage 1] fd2 reads: {os.read(fd2, 30)}")
    os.close(fd2)

    # Stage 2: spawn child, child opens parent's memfd via /proc/<parent_pid>/fd
    parent_pid = os.getpid()
    pid = os.fork()
    if pid == 0:
        # Child
        try:
            child_fd = os.open(
                f"/proc/{parent_pid}/fd/{fd}", os.O_RDWR
            )
            print(f"[stage 2 child] opened parent memfd as fd={child_fd}")
            # Stage 3: write through child's fd
            os.lseek(child_fd, 0, 0)
            os.write(child_fd, b"WRITTEN_FROM_CHILD")
            print("[stage 2 child] wrote 'WRITTEN_FROM_CHILD' through child's fd")
            os.close(child_fd)
            os._exit(0)
        except Exception as e:
            print(f"[stage 2 child] FAILED: {e}")
            os._exit(2)
    else:
        # Parent waits for child
        _, status = os.waitpid(pid, 0)
        if os.WEXITSTATUS(status) != 0:
            print(f"[parent] child exit status: {os.WEXITSTATUS(status)}")
            sys.exit(2)

    # Stage 3: parent reads back, should see child's write
    os.lseek(fd, 0, 0)
    data = os.read(fd, 40)
    print(f"[parent] post-child read: {data}")
    if b"WRITTEN_FROM_CHILD" in data:
        print("\nSUCCESS — cross-process memfd open via /proc/<pid>/fd/N works ✓")
        print("v0.4 can use this path to share memfd with Firecracker.")
        sys.exit(0)
    else:
        print("\nFAIL — child write not visible to parent")
        sys.exit(1)


if __name__ == "__main__":
    main()
