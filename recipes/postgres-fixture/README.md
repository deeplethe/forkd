# `postgres-fixture`

A forkd parent built from `postgres:16` with `initdb` already run and
the postmaster pre-launched. **Each child fork gets its own
isolated postgres ready to query in ~10 ms**, sharing the parent's
resident memory via CoW.

> **Status: working.** End-to-end verified on a bare-metal i7-12700
> dev box (Linux 6.14, KVM, 30 GiB): snapshot → fork 5 children with
> per-child netns → `psql -h 10.42.0.2 -U forkd -d forkd_test` from
> each child's netns returns its own row count. Children mutate
> their own postgres without affecting siblings (`/dev/shm` is per
> child VM, postmaster state diverges per page via mmap CoW).

## Why this recipe

The traditional alternative is "one postgres container per test,
killed on teardown" — which costs ~2 s of cold-start per test:

| Step | Cold-start cost |
|---|---:|
| Container start | ~300 ms |
| `initdb` | ~1.5 s |
| Postmaster ready-to-accept | ~200 ms |
| **Total per fresh database** | **~2 s** |

With forkd, `initdb` runs **once** at parent build; every fork
inherits the post-init state in ~10 ms. **200× speedup** at the
fixture level. Tests that needed a shared, mutable test database
because per-test cost was prohibitive can now go fully isolated.

## When to pick this

- **CI test suites** with hundreds of DB-touching integration tests
- **Per-PR preview environments** that need a fresh schema-aligned DB
- **Schema migration tests** — bring up the parent at the previous
  migration, fork N children, apply migrations in parallel,
  compare results
- **Fuzz testing** where each fuzz run wants a clean DB without
  paying initdb again
- **Multi-tenant SaaS dev/staging** with one warmed parent and
  per-tenant child databases

## What you get

- `postgres:16` Debian base (the postgres-image PATH expects bash
  process-substitution; Alpine is not supported)
- `python3` installed (required by forkd-init)
- `/dev/fd` symlinked to `/proc/self/fd` so docker-entrypoint's
  `initdb --pwfile=<(printf ...)` works
- Database **already initialised** at `/var/lib/postgresql/data`
- **Postmaster already running** in the parent at snapshot time
- Default user/db (override via env): user=`forkd`, password=`forkd`,
  database=`forkd_test`, trust auth on `0.0.0.0/0` (safe — postgres
  is only reachable inside the child netns)

Total rootfs: **~500 MB**.

## Use it

```bash
sudo bash recipes/postgres-fixture/build.sh
sudo bash scripts/host-tap.sh

sudo -E forkd snapshot --tag pgfix \
    --kernel ./vmlinux-6.1.141 \
    --rootfs recipes/postgres-fixture/parent.ext4 \
    --tap forkd-tap0 \
    --boot-wait-secs 15      # let initdb + postmaster settle

# Fork 20 isolated databases in parallel
sudo bash scripts/netns-setup.sh 20
sudo -E forkd fork --tag pgfix -n 20 --per-child-netns --memory-limit-mib 512

# Connect to one of them
psql -h 10.42.0.2 -p 5432 -U forkd -d forkd_test \
    -c "create table t (id int); insert into t values (1); select * from t;"
```

Each child sees its own postgres with the seeded `forkd_test` database,
trusted on the per-child netns. Schema/data writes diverge per child
via CoW.

### Customise credentials

```bash
PG_USER=app PG_PASSWORD=hunter2 PG_DATABASE=mydb \
    sudo -E bash recipes/postgres-fixture/build.sh
```

The values are baked into the parent rootfs at build time and exposed
to children via `/etc/forkd-recipe.env`.

### Schema seeding

To pre-load a schema into the parent (so every child inherits it):

```bash
# Drop your migration SQL into the rootfs before snapshot
sudo mount -o loop recipes/postgres-fixture/parent.ext4 /mnt/pg-parent
sudo cp my-schema.sql /mnt/pg-parent/docker-entrypoint-initdb.d/
sudo umount /mnt/pg-parent
```

Files in `/docker-entrypoint-initdb.d/` run during the first-boot
hook before the snapshot is taken (the recipe init script defers
to upstream's `docker-entrypoint.sh` for this).

## Python SDK

```python
from forkd import Sandbox
import psycopg

with Sandbox(tag="pgfix") as sb:
    # Each `with` block is a fresh child VM with its own postgres
    conn = psycopg.connect(
        host=sb.guest_addr.split(":")[0],
        port=5432,
        user="forkd",
        dbname="forkd_test",
    )
    with conn.cursor() as cur:
        cur.execute("CREATE TABLE users (id int PRIMARY KEY, name text)")
        cur.execute("INSERT INTO users VALUES (1, 'alice')")
        cur.execute("SELECT name FROM users WHERE id = 1")
        assert cur.fetchone()[0] == "alice"
    conn.close()
# Sandbox.__exit__ kills the child VM; postgres state goes with it.
```

## Caveats

- **`/dev/shm`** is per-child VM (independent kernel), so postgres
  shared buffers don't collide. But the postmaster process state in
  the parent's memory image is copied CoW — each child has the
  *same* postmaster PID at boot. That's fine because PIDs are per-
  PID-namespace.
- **WAL** — every write produces WAL entries. At high N (>100) you
  may want to crank `memory_limit_mib` per child to give postgres
  headroom for shared_buffers + WAL buffers.
- **Connections from outside the netns**: only one child VM is
  reachable at `10.42.0.2:5432` per netns. Make sure your client is
  inside the child's netns (`ip netns exec forkd-child-N ...`) or
  use the controller's exec/eval API.
- **Persistent volumes**: this recipe is for *ephemeral* test
  fixtures. For persistent storage across forks, attach a separate
  volume after the fork — that's a future recipe / extension.

## Performance ballpark

Measured on Linux 6.14 / 20 vCPU / 30 GiB / KVM with a 300 MB rootfs:

| N | Wall-clock (fork + ready-to-query) | Per-child |
|---:|---:|---:|
| 1 | ~50 ms | 50 ms |
| 10 | ~80 ms | 8 ms |
| 50 | ~250 ms | 5 ms |
| 100 | ~500 ms | 5 ms |

The "ready-to-query" measurement includes the round-trip needed to
`psql -c "SELECT 1"` from outside the netns. Pure VM fork is ~10 ms
per child; postgres re-accepts within a few ms after the child's
kernel finishes initialising the netns.
