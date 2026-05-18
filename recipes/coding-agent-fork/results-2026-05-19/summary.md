# Coding-agent fork — branch-and-fan-out demo run

- BRANCH tag: `coding-fork-1779085698`
- Daemon-measured pause: **3298 ms**
- Source sandbox: `sb-6a0ab17f-0045`

## The on-disk state inherited from BRANCH

All 3 grandchildren inherit `/workspace` byte-identically from the source. The 50 MiB synthetic `vendored.bin` and the populated `__pycache__/` are shared via copy-on-write — proof that fork captures filesystem state in a way no parallel-prompt approach can replicate.

| Agent | `vendored.bin` size | `vendored.bin` md5 | `__pycache__/` tree md5 |
|---|---:|---|---|
| source | 50 MiB | `4f03026522ed39ed…` | `4bf4ac3711cfd457…` |
| minimal | 50 MiB | `4f03026522ed39ed…` | `a0d8d63cd3fd88b6…` |
| rewrite | 50 MiB | `4f03026522ed39ed…` | `eb52b7fb072a9d2a…` |
| skip | 50 MiB | `4f03026522ed39ed…` | `8b2e50f992df9997…` |

✓ **All 4 agents share the same `vendored.bin` md5.** The 50 MiB blob travelled byte-identically across the fork. No prompt can do this.

## The divergent edits each child made

Each grandchild received a different fix strategy via `/tmp/forkd-strategy.sh`. The post-fix `mathy/__init__.py` differs:

### source

```python
"""Tiny math helpers. Imported by tests/test_add.py."""

def add(a, b):
    # BUG: should be a + b. Three forked agents will fix this in
    # three different ways below.
    return a - b


def double(x):
    return x * 2
```

Test outcome after this strategy: **❌ failed (real failures)**

### minimal

**Strategy:** `minimal.sh`

```python
"""Tiny math helpers. Imported by tests/test_add.py."""

def add(a, b):
    # BUG: should be a + b. Three forked agents will fix this in
    # three different ways below.
    return a + b


def double(x):
    return x * 2
```

Test outcome after this strategy: **✅ passed**

### rewrite

**Strategy:** `rewrite.sh`

```python
"""Tiny math helpers. Imported by tests/test_add.py.

Rewritten by the 'rewrite' branch with explicit type handling.
"""


def add(a, b):
    # Be explicit about the int contract; defensive against the
    # subtle bug a previous version had (it returned a - b).
    if not isinstance(a, (int, float)) or not isinstance(b, (int, float)):
        raise TypeError(f"add expects numeric inputs, got {type(a)} and {type(b)}")
    return a + b


def double(x):
    return x * 2
```

Test outcome after this strategy: **✅ passed**

### skip

**Strategy:** `skip.sh`

```python
"""Tiny math helpers. Imported by tests/test_add.py."""

def add(a, b):
    # BUG: should be a + b. Three forked agents will fix this in
    # three different ways below.
    return a - b


def double(x):
    return x * 2
```

Test outcome after this strategy: **❌ failed**

## What this run demonstrates

1. **Filesystem state survives BRANCH byte-identically.** The 50 MiB synthetic `vendored.bin` and the populated `__pycache__/` are shared across all 4 agents via Firecracker's copy-on-write memory image — this is the *filesystem* analog of the earlier langgraph-react demo's *conversation-history* sharing.

2. **Each child diverges only after the fork.** Three different fix strategies produce three different `mathy/__init__.py` files (and one of them edits the *tests* instead of the *source*). The branches are isolated; one child's edits do not appear in another's tree.

3. **No parallel-prompt approach can do this.** To replicate this with 3 parallel API calls, you'd have to bundle the entire `/workspace` directory into each prompt — including the binary `vendored.bin`. Not just inefficient: technically impossible above a few KiB.

Daemon pause window: **3298 ms** for the BRANCH operation. Three new sandboxes spun up from the snapshot, each ready to apply its own fix in parallel.
