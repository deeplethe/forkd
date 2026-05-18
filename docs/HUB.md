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
| `deeplethe/langgraph-react` | ReAct agent for the branch-and-fan-out demo (Python 3.12 + requests) | 513 MiB | 14.5 MiB |
| `deeplethe/coding-agent-fork` | Pre-warmed snapshot: `/tmp/workspace` already has the buggy mathy package, 50 MiB synthetic vendored.bin, `__pycache__` populated. Children boot 'ready to BRANCH'. | 513 MiB | 67.6 MiB |

More recipes (`postgres-fixture`, `python-numpy`, `agent-workbench`)
will land here as `recipes/<name>/build.sh` matures and we automate
the publishing pipeline.

The `coding-agent-fork` pack is intentionally larger than
`langgraph-react`: it carries a 50 MiB synthetic `vendored.bin` of
random bytes that zstd cannot compress. The point of including it is
to demonstrate that a pre-warmed snapshot can ship MiB-scale binary
state byte-identically to every child sandbox via copy-on-write,
which a parallel-prompt API call cannot replicate.

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
