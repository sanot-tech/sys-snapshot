# Contributing

Thanks for looking at this. It started as a tool for my own machine, so patches
that make it more correct or more portable are very welcome.

## Ground rules

**Zero dependencies is a feature, not an accident.** The whole program is
roughly 600 lines with nothing but the standard library. Reaching for a crate
to save fifteen lines makes the build slower and the binary harder to audit,
and this thing runs unattended as root-adjacent software every hour. If you
think a dependency is genuinely warranted, say so in the issue first.

**Comments must match the code.** Every non-obvious block carries a comment
explaining why, not what. If the code changes, the comment changes with it. A
comment that promises something the code does not do is worse than no comment
at all, because it makes the next reader trust a lie.

**Do not add emoji to README.md or any documentation.** Source comments are
plain text too, for a public repository.

## Before you open a pull request

```
cargo build --release
```

It must be warning free. No new warnings, no `#[allow]` to silence one.

If `cargo clippy` is available on your machine, please run it too:

```
rustup component add clippy
cargo clippy --release
```

## What a good patch looks like

- Fixes or explains the problem, ideally with the command output that shows it
- Keeps the dependency count at zero where possible
- Explains in the commit message why the old behaviour was wrong, not only
  what the new behaviour is. Future readers hit the same trap
- Updates README.md when behaviour or CLI changes

## Commit messages

Plain English, present tense, describing the reason before the mechanism.

```
Reject lower disk space before snapshotting

A snapshot that fills the disk can leave the machine unable to boot, which
is a worse outcome than having no snapshot at all. Skip the run below 2 GB
free instead of risking it.
```

## Testing a change

Take a snapshot, change something, take another, then confirm the second one
is actually incremental rather than a full copy:

```
./target/release/sys-snapshot --list
stat -c "%h %n" ~/.snapshots/*/home/.bashrc
```

Unchanged files should share an inode and show a link count above 1. Changed
files get their own inode. That check catches the self-copy recursion bug, which
is the most damaging thing that can go wrong here and it fails silently.

Then confirm it stayed quiet:

```
ionice -p $(pgrep rsync)
```

It should report `idle`.

## Reporting bugs

Open an issue with:

- what you ran
- what you expected
- what happened, including the exit code
- `rsync --version` and `uname -a`
- the output of `./target/release/sys-snapshot --list`

A rollback problem is the most serious kind. If you hit one, please say so in
the title, because those get looked at first.

## Security

Please report security issues privately rather than in a public issue. See
[SECURITY.md](SECURITY.md).

## Code of conduct

Be straightforward and technically honest. Assume the other person is trying to
help. Reviewing a patch is not a contest.