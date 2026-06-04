# forkd Hub

The Hub is forkd's namespace-resolved snapshot registry. It turns a
10-step recipe-building experience into a one-liner:

```bash
pip install forkd
forkd pull deeplethe/langgraph-react       # downloads the pack
sudo forkd fork --tag langgraph -n 3       # branches it
```

## How it works

The Hub is intentionally simple — no central service, no auth, no
cost. Three pieces:

1. **`registry.json`** at the root of this repo (`raw.githubusercontent.com/deeplethe/forkd/main/registry.json`)
   maps `<owner>/<name>` to a download URL + sha256.
2. **`.forkd-snapshot.tar.zst` packs** are attached to GitHub Releases
   with the tag scheme `hub-<name>-v<N>`. GitHub gives us free
   unlimited public-asset hosting.
3. **`forkd pull`** in the CLI fetches `registry.json`, looks up the
   package, downloads the asset, verifies sha256, unpacks into
   `$XDG_DATA_HOME/forkd/snapshots/<tag>/`.

Override the registry URL with `--hub <url>` or `FORKD_HUB_URL` if
you run your own (e.g., internal mirror, or your own fork's recipes).

## What's currently published

| Name | Description | Memory | Pack size |
|---|---|---:|---:|
| `deeplethe/python-numpy` | Python 3.12 + numpy preinstalled. Default fork-target for the README quickstart and bench scripts. | 1536 MiB | 15.8 MiB |
| `deeplethe/playwright-browser` | Node.js + Playwright 1.50 + Chromium pre-warmed with one `about:blank` tab. ~56 ms fork vs ~2-3 s cold container. | 2048 MiB | 105.5 MiB |
| `deeplethe/postgres-fixture` | PostgreSQL 16 with `initdb` done + postmaster pre-launched. ~10 ms fork-per-test vs ~2 s fresh container start. | 1024 MiB | 38.0 MiB |
| `deeplethe/coding-agent` | Python 3.12 + git + `gh` CLI + pytest. SWE-bench-style parallel evals — each child gets an isolated `git clone + pip install + pytest`. | 1024 MiB | 15.2 MiB |
| `deeplethe/nodejs` | Node.js 22 slim runtime preloaded. Generic JS workload base. | 512 MiB | 10.5 MiB |
| `deeplethe/e2b-codeinterpreter` | E2B code-interpreter image preloaded (Python + Node + Jupyter + ML libs). Drop-in for the E2B sandbox API via forkd's Python SDK. | 2048 MiB | 21.6 MiB |
| `deeplethe/jupyter-kernel` | Jupyter scipy-notebook with IPython + numpy + scipy + pandas + matplotlib pre-imported. | 2048 MiB | 15.5 MiB |
| `deeplethe/langgraph-react` | ReAct agent for the branch-and-fan-out demo (Python 3.12 + requests). | 513 MiB | 14.5 MiB |
| `deeplethe/coding-agent-fork` | Pre-warmed: `/tmp/workspace` with the buggy mathy package, 50 MiB synthetic `vendored.bin`, populated `__pycache__/`. Children boot 'ready to BRANCH'. | 513 MiB | 67.6 MiB |

The `coding-agent-fork` pack is intentionally larger than
`langgraph-react`: it carries a 50 MiB synthetic `vendored.bin` of
random bytes that zstd cannot compress. The point of including it is
to demonstrate that a pre-warmed snapshot can ship MiB-scale binary
state byte-identically to every child sandbox via copy-on-write,
which a parallel-prompt API call cannot replicate.

## Pack format

Hub bundles are zstd-compressed tarballs (`.forkd-snapshot.tar.zst`)
with a TOML manifest at the root. Two on-the-wire layouts coexist:

### v1 — single snapshot (pre-v0.5, still the default for bases)

```text
manifest.toml
snapshot.json
vmstate
memory.bin
rootfs.ext4         # optional
```

`manifest.toml` carries `forkd_pack_version = 1` plus per-file
`sha256` digests; `unpack` verifies every file against the digest
after extraction.

### v2 — chained snapshot (v0.5+)

Emitted when `forkd pack` is invoked on a snapshot whose
`snapshot.json` has `parent_tag` set. The bundle includes every
ancestor — `unpack` materializes one snapshot directory per chain
link.

```text
manifest.toml          # forkd_pack_version = 2, chain[] = root → head
<tag-0>/snapshot.json   ┐
<tag-0>/vmstate         │ root base
<tag-0>/memory.bin      │
<tag-0>/rootfs.ext4     ┘
<tag-1>/...             # first diff link
<tag-2>/...             # head
```

Manifest's `chain` field is an array of `ChainLinkMeta` ordered
root → head:

```toml
forkd_pack_version = 2
tag = "deeplethe/py-pandas"        # the head's name
parent_tag = "deeplethe/py-numpy"  # legacy mirror of chain.last().parent_tag

[[chain]]
tag = "py-base"                    # root base
files = [
  { path = "snapshot.json", size = 179,       sha256 = "..." },
  { path = "vmstate",       size = 29436,     sha256 = "..." },
  { path = "memory.bin",    size = 536870912, sha256 = "b356ee89..." },
]

[[chain]]
tag = "py-numpy"
parent_tag = "py-base"
parent_content_hash = "b356ee89..."  # SHA-256 of parent's memory.bin
files = [ ... ]                       # paths relative to <tag>/ in the tar

[[chain]]
tag = "py-pandas"
parent_tag = "py-numpy"
parent_content_hash = "..."
files = [ ... ]
```

**Back-compat invariant:** v0.5 `forkd` clients accept both v1 and
v2 packs (`MAX_SUPPORTED_PACK_VERSION = 2`). Older clients reject v2
with a clear "newer format" error rather than silently
mis-extracting. The top-level `parent_tag` field on v2 manifests
mirrors `chain.last().parent_tag` so v1 readers peeking at it before
the version check still see something meaningful.

**Safety:** `unpack` validates every `chain[].tag` against
alnum/dash/underscore rules **before** extracting any file body, so
a malicious bundle declaring `tag = "../etc"` is refused at
manifest-parse time. Multi-link bundles refuse the `--tag <override>`
flag (ambiguous which link to retag); single-link bundles accept it
for symmetry with v1.

## Publishing a new pack

```bash
# 1) Build your snapshot locally (see recipes/<name>/build.sh)
sudo forkd snapshot --tag mything --kernel ... --rootfs ...

# 2) Pack it
sudo HOME=$HOME forkd pack \
    --tag mything \
    --description "what this is" \
    --base-image python:3.12-slim \
    --out /tmp/mything.forkd-snapshot.tar.zst

# 3) sha256 + size
sha256sum /tmp/mything.forkd-snapshot.tar.zst
wc -c   /tmp/mything.forkd-snapshot.tar.zst

# 4) Create the GitHub release
gh release create hub-mything-v1 \
    /tmp/mything.forkd-snapshot.tar.zst \
    --target main \
    --title "Hub: <yourorg>/mything v1" \
    --notes "..."

# 5) Add an entry to registry.json:
#    - "url" = the release asset download URL
#    - "sha256" = the hex digest from step 3
#    - "size_bytes" = the byte count from step 3
#
# 6) Open a PR to deeplethe/forkd updating registry.json.
#    Once merged, your pack is `forkd pull <yourorg>/mything`-able.
```

## Schema (`registry.json`)

```jsonc
{
  "schema_version": 1,
  "packages": {
    "<owner>/<name>": {
      "description": "human-readable, shows up in `forkd images --hub`",
      "versions": {
        "<version>": {
          "url":         "https://...",     // required, download URL
          "sha256":      "<hex digest>",    // optional but recommended
          "size_bytes":  12345,             // optional, used for progress estimates
          "memory_mib":  513,               // optional, expected guest RAM
          "base_image":  "python:3.12-slim",// optional, audit trail
          "recipe_path": "recipes/<name>",  // optional, source-of-truth recipe
          "created_at":  "2026-05-18T...",  // optional, ISO 8601
          "release_tag": "hub-<name>-v1"    // optional, audit trail
        }
      }
    }
  }
}
```

Every package must have a `"latest"` version. Additional named
versions (`"v1"`, `"v2"`, ...) let users pin via
`forkd pull <owner>/<name>@v1`.

## Security model

- **Public read.** Anyone can pull any pack listed in `registry.json`.
- **Push via PR.** Adding a pack means opening a PR to this repo. A
  maintainer reviews the URL + sha256 + the recipe that produced it.
- **No signing yet.** The sha256 in `registry.json` is integrity, not
  authenticity. v0.x is OSS-first, single-trust-domain (this repo);
  v1.0 will add Sigstore / cosign signatures.
- **Trust model.** Pulling a pack and running it as a sandbox = trusting
  the publisher. The pack contains a guest kernel image + rootfs that
  will be booted under KVM. If the publisher is hostile, KVM is the
  only thing between them and your host. forkd's threat model is no
  weaker than "you ran a Docker image from this registry" — but also no
  stronger.

## Why GitHub Releases?

It's the cheapest, most reliable distribution layer for a v0.x OSS
project:

- **Free.** Public repos have unlimited release asset storage and
  bandwidth.
- **Stable.** Asset URLs don't rotate. Once published, the URL works
  forever.
- **2 GiB per file.** Our biggest current pack is 15 MiB; the largest
  realistic forkd pack (a 4 GiB warm parent) compresses to ~300 MiB.
  Comfortably under.
- **No vendor lock-in.** If we outgrow it, change the `url` field in
  `registry.json`; clients don't have to upgrade.

If you need higher-volume / private hosting, run your own registry
that serves a `registry.json` and point `FORKD_HUB_URL` at it. The
client doesn't care.
