# Security Policy

## Supported versions

Only the latest release on `main` receives fixes.

## Reporting a vulnerability

Please use GitHub's private vulnerability reporting rather than opening a
public issue. You will normally get an acknowledgement within a few days.

Include enough to reproduce: the command, the observed behaviour, and your
distribution.

## Threat model

This program is worth thinking about before trusting it with an hourly slot on
your machine.

**What it does:** reads `/etc`, the home directory and the package database, and
writes copies into `~/.snapshots`, owned by the user running it.

**What it cannot do:** modify the system. It has no code path that writes
outside its own store. It does not ask for root, does not use `sudo`, and does
not open a socket. Running it as an unprivileged account limits the damage a
compromise could do to your data alone.

**What it asks of you at rollback time:** `sudo`. The printed rollback commands
carry root rights, so read them before pasting them. The tool deliberately
refuses to run them itself, because restoring `/etc` from a snapshot that turns
out to be bad is an excellent way to lose a machine.

## Things to keep in mind

**Snapshots contain your data in the clear.** `~/.snapshots` is a full copy of
your home directory. If it holds SSH keys, browser profiles or API tokens, a
copy exists there that nothing else protects. It is created with your umask, so
it inherits whatever your default is. Consider tightening the mode on the store
directory if your `~/.snapshots` ends up world-readable.

**Snapshots are not encrypted.** Anything that can read your home directory can
read your snapshots.

**Old snapshots are pruned automatically.** The 24 most recent are kept. That is
enough for rolling back a bad change within a day, and not enough to keep a
history. Raise `KEEP_SNAPSHOTS` in `src/main.rs` if you need more, and watch
free space.

**Not crash-consistent.** A snapshot captured while the system is mid-update can
record files in a half-written state. Restoring it may reproduce the broken
state rather than avoid it.

## Rollback safety

Always take the pre-rollback backup printed in step 2 before restoring
anything. Restoring is reversible only if you keep it.