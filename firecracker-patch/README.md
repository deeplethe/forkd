# Firecracker patch for forkd v0.3

forkd v0.3 (live-branching via uffd_wp) needs Firecracker to accept an
externally-created `memfd_create(2)` file descriptor as the guest RAM
backing. Upstream Firecracker v1.10.1 has no public API for this — the
existing `MemoryBackend` enum exposes only `File` and `Uffd`. We
maintain a small patch on top of the v1.10.1 tag.

This directory holds **the patch design and apply instructions**. The
actual patched binary is published as a release asset on
`deeplethe/firecracker` (the fork repo). `scripts/setup-host.sh`
downloads it when `FORKD_FIRECRACKER_BUILD=forkd-v0.3` is set; the
default still pulls vanilla upstream.

## Why patch and not work around

Researched in `docs/design/userfaultfd.md` § "Open question 1". Brief:

- No CLI flag, env var, or API field on v1.10.1 accepts an external fd
  for guest memory.
- `/proc/self/fd/<n>` path injection technically loads but breaks
  under the jailer's mount namespace; you end up needing SCM_RIGHTS
  plumbing anyway and saving nothing.
- Firecracker's memory module **already has** the memfd machinery
  in-tree (`GuestMemoryMmap::memfd_backed`,
  `src/vmm/src/vstate/memory.rs:127–147`), gated only by the
  vhost-user check. Adding a third `MemoryBackend` variant that
  takes the fd over the existing UDS handshake is ~100 LOC of
  plumbing on top of code that's already correct.

Prior art: CodeSandbox patched Firecracker for the same purpose
(two public blog posts) and never upstreamed. Their wire format is
roughly the same shape as what's described below.

## Patch shape (pseudo-diff)

### 1. Swagger enum (`src/firecracker/swagger/firecracker.yaml`)

```yaml
   MemoryBackend:
     type: object
     properties:
       backend_type:
         type: string
-        enum: [File, Uffd]
+        enum: [File, Uffd, Memfd]
       backend_path:
         type: string
+        description: |
+          For File: path to memory.bin.
+          For Uffd: path to a UDS the handler is listening on.
+          For Memfd: path to a UDS the handler is listening on; the
+          handler sends BOTH the uffd fd AND the memfd as SCM_RIGHTS
+          ancillary data on a single sendmsg.
```

### 2. Rust enum (`src/vmm/src/persist.rs`)

```rust
   pub enum MemBackendType {
       File,
       Uffd,
+      Memfd,
   }
```

### 3. UDS receive path

The existing UFFD UDS handshake lives in
`src/vmm/src/vstate/memory.rs::create_userfaultfd_handler` (approximate
location — verify by reading the file when forking). It already does
`recvmsg` + parses `SCM_RIGHTS` for the uffd fd. We extend it to:

- Accept **two** fds in `SCM_RIGHTS` instead of one when
  `backend_type == "Memfd"`. Order: `[memfd, uffd]`.
- Pass the memfd to `GuestMemoryMmap::from_external_memfd` (new
  helper, ~30 LOC, reuses `from_raw_regions_file`).
- Keep the uffd path identical to today's `Uffd` backend so
  uffd_wp / UFFDIO_COPY semantics from forkd's handler crate work
  unchanged.

### 4. New helper in `src/vmm/src/vstate/memory.rs`

```rust
   impl GuestMemoryMmap {
+      /// Build guest memory by mapping an externally-supplied memfd
+      /// (passed via SCM_RIGHTS at snapshot/load time). The caller has
+      /// already created the memfd, populated it with snapshot bytes,
+      /// and sealed it with F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_SEAL.
+      /// Used by forkd v0.3 live-branching to give N children
+      /// MAP_PRIVATE views of the same physical pages.
+      pub fn from_external_memfd(
+          memfd: OwnedFd,
+          mem_state: &GuestMemoryState,
+          track_dirty_pages: bool,
+      ) -> Result<Self, MemoryError> {
+          let file = File::from(memfd);
+          let regions: Vec<_> = mem_state.regions.iter()
+              .map(|r| (r.size, GuestAddress(r.base_address)))
+              .collect();
+          Self::from_raw_regions_file(
+              &regions,
+              FileOffset::new(file, 0),
+              track_dirty_pages,
+              /*shared=*/false, // MAP_PRIVATE for CoW fan-out
+          )
+      }
   }
```

### 5. Dispatch in `guest_memory_from_file`

```rust
   match mem_backend.backend_type {
       MemBackendType::File => { /* unchanged */ },
       MemBackendType::Uffd => { /* unchanged */ },
+      MemBackendType::Memfd => {
+          let handshake = recv_memfd_uffd_handshake(&uds_path)?;
+          let mem = GuestMemoryMmap::from_external_memfd(
+              handshake.memfd, mem_state, track_dirty_pages)?;
+          // The uffd is still registered against the memfd-backed
+          // regions so uffd_wp + write-protect-on-source-write works.
+          register_uffd(mem, handshake.uffd)?;
+          Ok(mem)
+      },
   }
```

## Apply instructions (once the fork repo exists)

```bash
# One-time fork setup:
git clone git@github.com:firecracker-microvm/firecracker.git deeplethe-firecracker
cd deeplethe-firecracker
git checkout -b forkd-v0.3 v1.10.1
# Apply the patch by hand (no .patch file ships yet — see "TODO" below).
# Validate:
tools/devtool build --release
./build/cargo_target/x86_64-unknown-linux-musl/release/firecracker \
    --version | grep -q forkd-v0.3
# Publish release:
git push origin forkd-v0.3
gh release create forkd-v0.3-rc1 \
    build/cargo_target/x86_64-unknown-linux-musl/release/firecracker
```

Then in this repo:

```bash
# scripts/setup-host.sh switches FC_VERSION + download URL conditional
# on FORKD_FIRECRACKER_BUILD=forkd-v0.3 — that change is part of phase 2
# in this repo, NOT in the firecracker fork.
```

## TODO

- [ ] Create `deeplethe/firecracker` repo (empty fork of upstream).
- [ ] Read actual `src/vmm/src/vstate/memory.rs` and
      `src/vmm/src/persist.rs` at the v1.10.1 tag; verify the function
      names / line numbers above match. Update this doc.
- [ ] Write the real patch as a single `.patch` file in this dir
      (`v0.3-memfd-backend.patch`) so it can be `git apply`'d.
- [ ] Add a unit test in firecracker that creates a memfd, sends it
      over a socketpair, and asserts the resulting `GuestMemoryMmap`
      maps the expected bytes.
- [ ] Update `scripts/setup-host.sh` to switch on
      `FORKD_FIRECRACKER_BUILD` env var.
- [ ] Update `docs/design/userfaultfd.md` § "Phase 2" once the patch
      actually lands and we have measured numbers.

The pseudo-diff above is the design — the real diff is a 1-week piece
of work, blocked on access to a dev box where `tools/devtool build`
can run (the build is hermetic-musl-via-docker; takes ~10 min).
