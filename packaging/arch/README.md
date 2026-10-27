# Arch Linux package

`sandstorm-bin` installs the official prebuilt sandstorm release and depends on the
distribution `firecracker` package. It never downloads or replaces
Firecracker.

Build locally:

```bash
cd packaging/arch
makepkg --cleanbuild
sudo pacman -U sandstorm-bin-*.pkg.tar.zst
```

Before enabling the controller, create a bearer token:

```bash
sudo install -d -m 0755 /etc/sandstorm
sudo sh -c 'umask 077; head -c 32 /dev/urandom | base64 > /etc/sandstorm/token'
sudo systemctl enable --now sandstormd
```

The service stores snapshots in `/var/lib/sandstorm/snapshots`.

In v0.5.3, `quickstart` can stage its embedded helper scripts, but
`parent build` and `from-image` still require `scripts/build-rootfs.sh` from a
sandstorm source checkout. Point `SANDSTORM_SCRIPTS_DIR` at `<checkout>/scripts`
before running either command. In fish:

```fish
set -gx SANDSTORM_SCRIPTS_DIR /path/to/sandstorm/scripts
```

Snapshots are not portable across Firecracker versions. Recreate them after a
Firecracker upgrade. The standard Arch Firecracker supports normal fork,
full branch, and diff branch. It does not include sandstorm's experimental patches
for `live_fork=true` or `mode="live"`.

Publishing the recipe to AUR requires a separate AUR Git repository and a
maintainer identity. This directory is the upstream source of that recipe.
