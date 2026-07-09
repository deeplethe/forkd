# KSM memfd probe

This probe checks whether `MADV_MERGEABLE` produces KSM sharing for
memfd-backed `MAP_SHARED` regions.

Why this exists: issue #5 asks for directed KSM hints for fork families.
For the v0.4 live-fork path the tempting implementation is to mark the
controller's memfd mapping mergeable. The Linux KSM documentation says
KSM targets private anonymous pages, though, so this benchmark keeps the
evidence next to the code before forkd grows a misleading hint.

## Environment

- Run from Docker with `--privileged`
- Host KSM sysfs writable at `/sys/kernel/mm/ksm`
- Probe command:

```bash
docker run --rm --privileged \
  -v "$PWD:/work" -w /work \
  python:3.12-slim \
  bash -lc 'python bench/ksm-memfd/probe.py --pages 4096 --timeout 30'
```

The probe saves and restores `run`, `pages_to_scan`, and
`sleep_millisecs`. Before each case it writes `run=2` to unmerge any
existing KSM pages, waits for `pages_sharing=0`, then enables KSM and
waits for scans.

## Result

```text
anonymous-private
  before={'pages_shared': 0, 'pages_sharing': 0, 'pages_unshared': 0, 'pages_volatile': 0, 'full_scans': 0}
  after ={'pages_shared': 2, 'pages_sharing': 510, 'pages_unshared': 0, 'pages_volatile': 5069, 'full_scans': 2}
  delta ={'pages_shared': 2, 'pages_sharing': 510, 'pages_unshared': 0, 'pages_volatile': 5069, 'full_scans': 2}
memfd-map-shared-two-fds
  before={'pages_shared': 0, 'pages_sharing': 0, 'pages_unshared': 0, 'pages_volatile': 0, 'full_scans': 0}
  after ={'pages_shared': 0, 'pages_sharing': 0, 'pages_unshared': 0, 'pages_volatile': 0, 'full_scans': 0}
  delta ={'pages_shared': 0, 'pages_sharing': 0, 'pages_unshared': 0, 'pages_volatile': 0, 'full_scans': 0}
```

The control case confirms KSM is active and can merge identical
`MADV_MERGEABLE` anonymous private pages. The memfd `MAP_SHARED` case
does not move `pages_shared` or `pages_sharing`, so forkd should not add a
controller-side memfd `MADV_MERGEABLE` mapping as if it fixed #5.

The useful part of #5 that remains in this PR is the `forkd doctor` KSM
check. A future implementation should target a backing/mapping shape that
KSM actually scans, and should include this probe (or a forkd-specific
variant of it) in the evidence.
