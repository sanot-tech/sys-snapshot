# sys-snapshot

Hourly system snapshots with a rollback path, in about 600 lines of Rust with
zero dependencies.

Most Linux filesystems have no snapshot feature. ext4, which is what most
distributions ship, offers nothing of the kind, and setting up LVM or btrfs
subvolumes needs the root password. `sys-snapshot` gives you rolling hourly
snapshots anyway, using `rsync` hard links, and prints you the exact commands
to roll back.

It runs as your normal user. It never touches the system, only reads it.

```
$ sys-snapshot --list
Snapshots: 24 (keeping the most recent 24)

  TOTAL: 1.4 GB  (shared files counted once)

  20261008-140312   /home/you/.snapshots/20261008-140312
  20261008-130214   /home/you/.snapshots/20261008-130214
  20261008-120005   /home/you/.snapshots/20261008-120005
  ...

Rollback: sys-snapshot --restore <ID>
```

## Why it is fast

Snapshots are incremental. An unchanged file is stored once and referenced by
hard link from every snapshot, so the second snapshot costs roughly the size of
what actually changed.

Measured on a machine with 20 GB of watched files:

| Operation | Time |
| --- | --- |
| First snapshot (full copy) | a few minutes |
| Every later snapshot | about 1 second |
| 24 snapshots of that machine | 1.4 GB total |

## Why it is quiet

A background job has to be invisible. Everything here is built for that:

- Runs under `nice 19` and `ionice -c3`, so it yields to any interactive work
  and touches the disk only while the disk is otherwise idle
- Caps bandwidth at 20 MB/s so a snapshot cannot saturate a Wi-Fi link
- Refuses to start when free space drops below 2 GB, because a snapshot that
  fills the disk is worse than no snapshot at all
- Takes a lock file so two runs can never race each other

Verify it yourself while it runs:

```
$ ionice -p $(pgrep rsync)
idle
```

## Why it needs no root

Snapshots are taken with `rsync --fake-super`. That stores file ownership in
extended attributes instead of in the real `uid`/`gid`, which means snapshot
files belong to you and can be pruned without `sudo`. Rollback restores the
real ownership with `rsync --super`, and you run that yourself.

A background daemon that silently writes to `/etc` with root rights is a risk
nobody needs.

Some files under `/etc` cannot be read without root (`ssl/private`, `shadow`,
`sudoers.d`). Those are skipped, and `rsync` exiting with code 23 is reported
as a partial snapshot rather than a failure, because refusing to read them is
the correct behaviour for an unprivileged process.

## Install

```
git clone https://github.com/sanot-tech/sys-snapshot
cd sys-snapshot
cargo build --release
sudo install -m755 target/release/sys-snapshot /usr/local/bin/
```

Requires `rsync`, and Rust 1.75 or newer to build.

## Hourly scheduling with systemd

The timer deliberately does not fire exactly on the hour.

```
~/.config/systemd/user/sys-snapshot.service
~/.config/systemd/user/sys-snapshot.timer
```

```
systemctl --user daemon-reload
systemctl --user enable --now sys-snapshot.timer
systemctl --user list-timers sys-snapshot.timer
```

Two details matter:

`RandomizedDelaySec=300` adds up to five minutes of random delay. Everything
scheduled with `OnCalendar=hourly` fires at `:00`, so without this your
snapshot would compete with every other timer on the machine for the same
disk.

`OnCalendar=hourly` rather than `OnUnitActiveSec=1h`. The latter counts an hour
from the last run and has no starting point before the first one, so a freshly
enabled timer reports `NEXT=-` and silently does nothing.

Run `loginctl enable-linger "$USER"` so the timer keeps working after you log
out.

## Rolling back

```
$ sys-snapshot --restore 20261008-140312
Rollback to snapshot 20261008-140312 - /home/you/.snapshots/20261008-140312

Nothing is applied automatically. Inspect first, then decide.

# 1. See what is actually in the snapshot:
ls -la /home/you/.snapshots/20261008-140312/etc /home/you/.snapshots/20261008-140312/home

# 2. FIRST back up the current state, so the rollback is reversible:
sudo mkdir -p /tmp/pre-rollback
sudo rsync -a /etc/ /tmp/pre-rollback/etc/

# 3. Restore a single file (safe, start here):
sudo rsync -a --super /home/you/.snapshots/20261008-140312/etc/fstab /etc/fstab

# 4. Restore a directory (overwrites current contents):
sudo rsync -a --super /home/you/.snapshots/20261008-140312/etc/ /etc/
sudo rsync -a --super /home/you/.snapshots/20261008-140312/home/ /home/

# 5. Reload if you touched systemd:
sudo systemctl daemon-reload && sudo reboot
```

The tool prints commands and never runs them. Restoring `/etc` from a snapshot
that turns out to be bad is a good way to destroy a system, so that decision
stays with you.

`--super` is not optional. Snapshots were taken with `--fake-super`, so the real
owner lives in an xattr while the file on disk is yours. Without `--super`,
rsync ignores that xattr and recreates `/etc` files owned by your user account,
and the system stops working.

## What gets snapshotted

| Source | Why |
| --- | --- |
| `/etc` | Network config, sudoers, systemd units, fstab |
| `~/` | Configs, documents, projects |
| `/var/lib/pacman` | Without it a removed package cannot be put back |

Excluded, because they are large and do not change between snapshots:

`.snapshots/`, `.cache/`, `.rustup/`, `.cargo/`, `tool-output/`, `Trash/`,
`~/target`, and the `/etc` paths a normal user cannot read.

`/usr` is not snapshotted. It is 9 GB that only changes when a package is
installed, and `pacman` already has the history to undo that. Snapshot it only
if you want something your package manager does not cover.

To change the list, edit `WATCH_SYSTEM` and `excludes()` in `src/main.rs`.

## A trap worth knowing about

rsync matches exclude patterns **relative to the transfer root**, not as
absolute filesystem paths.

For `rsync /home/you/ dst/`, the transfer root is `/home/you/`, so
`/home/you/.snapshots` appears to rsync as `/.snapshots`. The pattern
`/home/you/.snapshots` therefore matches nothing at all: the leading slash
means "anchor to the transfer root", so rsync looks for
`/home/you/home/you/.snapshots`, which does not exist.

The failure is silent and expensive. The snapshot copies itself into itself,
each snapshot is much larger than the last, and the disk fills within hours.

Write patterns without a leading slash and with a trailing slash, such as
`.snapshots/`, which matches a directory of that name at any depth.

## Storage layout

```
~/.snapshots/
  20261008-130214/
    etc/          # /etc
    home/         # ~/
    pacman/       # /var/lib/pacman
  20261008-140312/
    ...
```

Names are UTC timestamps, which keeps them sortable and immune to daylight
saving changes reordering snapshots. The 24 most recent are kept and older ones
are pruned automatically.

## Exit codes

| Code | Meaning |
| --- | --- |
| 0 | Snapshot taken, or command completed |
| 1 | Snapshot incomplete, or bad arguments |
| 2 | Not applicable, reserved |

## Limitations

- Not crash-consistent. A snapshot taken while the system is being configured
  may capture files half-written. Good enough for hourly safety nets, not a
  substitute for a real block-level snapshot.
- Does not cover `/usr`. Package upgrades are `pacman`'s job.
- Needs the same filesystem for the store and the originals, otherwise hard
  links are impossible and every snapshot becomes a full copy. The default
  store under `~` satisfies this.
- Only Linux, and only where the `nice` and `ionice` binaries exist.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). `cargo build --release` must be warning
free, and new behaviour needs a test or a measurement behind it.

## License

MIT. See [LICENSE](LICENSE).