# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and this project uses
[semantic versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0] - 2026-10-07

### Added

- Incremental hourly snapshots of `/etc`, the home directory and the package
  database, keeping the last 24.
- `--list` to show available snapshots and the total space they use.
- `--restore <ID>` which prints rollback commands with real paths filled in.
  It does not apply them and does not ask for root.
- systemd user service and timer, with `RandomizedDelaySec` so snapshots do not
  all fire on the hour, and `Persistent=true` so a snapshot is taken after the
  machine was powered off.
- A lock file so two concurrent runs cannot corrupt each other's hard links.

### Changed

- Snapshot size is now measured across the whole store at once. `du` does not
  deduplicate hard links between separate arguments, so the previous
  per-snapshot number reported the full apparent size of every snapshot and
  made a few-megabyte delta look like 600 MB.

### Fixed

- **Snapshots copied themselves into themselves.** rsync matches exclude
  patterns relative to the transfer root, not as absolute paths, so
  `/home/user/.snapshots` never matched anything. The first snapshot was 2.4 GB
  instead of 340 MB, and repeated runs would have filled the disk. The pattern
  is now `.snapshots/`, and the tool refuses to run if it is missing.
- **Snapshots could not be deleted.** Plain `-a` preserved root ownership, so
  files copied from `/etc` came out owned by root and pruning failed with
  "Permission denied". `--fake-super` now stores ownership in xattrs, so the
  store belongs to the user and rolls back with `rsync --super`.
- **Partial transfers were reported as failures.** rsync exit code 23 is normal
  for a non-root reader hitting `/etc/ssl/private` and friends. It is now a
  warning, and the snapshot counts as taken.
- **The recursion guard lied.** It printed that it was skipping a source and
  then copied it anyway. It now asserts that the exclude pattern is present and
  exits rather than starting a self-copying snapshot.
- **The rollback instructions were wrong.** The pre-rollback backup was written
  inside the snapshot it was meant to protect, and restoring without `--super`
  would have recreated `/etc` files owned by the user account, breaking the
  system.

## [0.1.0] - 2026-10-07

Initial working version: a single full copy per run with no deduplication.
Replaced in the same day once hard linking and the exclude rules were
understood.

[Unreleased]: https://github.com/sanot-tech/sys-snapshot/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/sanot-tech/sys-snapshot/releases/tag/v0.2.0