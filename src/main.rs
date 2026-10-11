// ============================================================================
//  sys-snapshot - hourly system snapshots with rollback
// ----------------------------------------------------------------------------
//  WHAT THIS IS
//    Takes an incremental snapshot of the files that actually change on a
//    live system, keeps a rolling history, and prints the exact commands to
//    roll back. It never applies a rollback by itself.
//
//  WHY rsync AND NOT A REAL SNAPSHOT MECHANISM
//    Most root filesystems are ext4, which has no snapshot feature at all,
//    and setting up LVM or btrfs subvolumes needs the root password. rsync
//    with --link-dest gets us incremental snapshots anyway: an unchanged
//    file is stored once and referenced by hard link from every snapshot.
//    Measured on this machine: a full first snapshot takes minutes, every
//    later snapshot takes about one second.
//
//  WHY NOT A CUSTOM STORAGE FORMAT
//    rsync has done incremental snapshots for twenty years and is updated
//    with the distribution. Hand-rolling a format means writing code that
//    will be debugged at 3am the first time it corrupts something. A small
//    program that calls a well-tested tool is the more reliable choice.
//
//  OPERATING WITHOUT ROOT, ON PURPOSE
//    Snapshots are taken as an ordinary user:
//      --fake-super stores ownership in xattrs instead of in the real
//      uid/gid, so snapshot files stay owned by the current user and can be
//      deleted and pruned without sudo. Rollback re-applies the real owner
//      with "sudo rsync --super", and it is the operator who runs it.
//    A background daemon silently writing to /etc with root rights is a
//    risk nobody needs. This process only ever reads the system.
//
//  QUIET BY DESIGN
//    A background job must be invisible. Everything here is deliberately
//    quiet:
//      - runs under nice 19 and ionice idle class, so it yields to any real
//        work and only touches the disk when the disk is otherwise idle
//      - caps bandwidth, so a snapshot cannot saturate a Wi-Fi link
//      - refuses to start when free space is low
//      - takes a lock file so two runs can never race
//    Verified, not just intended: `ionice -p <pid>` reports "idle".
//
//  USAGE
//      sys-ssnapshot                  take a snapshot now
//      sys-snapshot --list            list available snapshots
//      sys-snapshot --restore <ID>    print rollback commands for a snapshot
//
//  BUILD
//      cargo build --release
//      systemd --user enable --now sys-snapshot.timer   (see examples/)
// ============================================================================

use std::env; // command line arguments, environment
use std::fs; // directory and file operations
use std::path::{Path, PathBuf}; // path handling
use std::process::{Command, Stdio}; // running rsync and friends
use std::time::{SystemTime, UNIX_EPOCH}; // timestamp for the snapshot name

// ===========================================================================
//  CONFIGURATION
//
//  Everything tunable lives in this one section. Paths are derived from $HOME
//  rather than hardcoded, so the tool works on any account and nothing about
//  the host leaks into the repository.
// ===========================================================================

/// Directory where snapshots are stored.
///
/// Must be on the SAME filesystem as the originals, otherwise hard links
/// are impossible and every snapshot costs a full copy.
///
/// It also sits inside the home directory by default, which is a trap worth
/// naming: the home directory is one of the things being snapshotted, so the
/// store has to be excluded explicitly. See EXCLUDES below.
fn snapshot_root() -> PathBuf {
    let home = env::var("HOME").unwrap_or_else(|_| "/root".into());
    PathBuf::from(home).join(".snapshots")
}

/// The home directory being snapshotted.
fn home_dir() -> PathBuf {
    let home = env::var("HOME").unwrap_or_else(|_| "/root".into());
    PathBuf::from(home)
}

/// System paths to snapshot, as (directory name inside a snapshot, source).
///
/// The name is part of the on-disk layout and must stay stable across
/// versions: it is what --link-dest resolves against. Renaming an entry
/// orphans the hard-link base of the previous snapshots and silently turns
/// the next snapshot into a full copy, so pick names once and keep them.
///
/// Only things that actually change between snapshots belong here. A full
/// copy of /usr is 9 GB of files that only move when a package is installed,
/// and pacman already handles that case.
const WATCH_SYSTEM: &[(&str, &str)] = &[
    // System configuration. The most valuable thing to roll back: network
    // settings, sudoers, systemd units, fstab. Changes rarely, weighs 14 MB.
    ("etc", "/etc"),
    // Package database state. Without it a removed package cannot be put
    // back except by reinstalling it by hand.
    ("pacman", "/var/lib/pacman"),
];

/// Number of snapshots to keep.
///
/// Was 24, on the reasoning that 24 hourly snapshots is roughly one day of
/// rollback. That reasoning ignores what a snapshot costs.
///
/// Measured on this machine: each hourly snapshot consumes roughly 4.7 GB, so
/// a day of history is about 110 GB on a 117 GB filesystem. The retention was
/// not "one day of rollback", it was "one day until the disk is full and the
/// machine cannot boot". It filled on its own, without any of the build activity
/// that could reasonably be blamed for it.
///
/// 6 keeps six hours of hourly history -- long enough to cover a working
/// session and a night's sleep -- at about 28 GB, which leaves room for a
/// Rust build target, package caches and whatever else the machine is doing.
///
/// The real fix is not this constant. It is [`MIN_FREE_MB`] below, which is
/// checked on every run and is what actually stops the disk filling. Retention
/// only decides how much history survives once that guard holds.
const KEEP_SNAPSHOTS: usize = 6;

/// Skip the snapshot when free space drops below this, in megabytes.
///
/// A snapshot that fills the disk is worse than no snapshot at all: it can
/// leave the machine unable to boot. That part was always right.
///
/// What was wrong was the size. 2 GB looked like a comfortable margin and is
/// less than half a single snapshot here. The guard could therefore be satisfied
/// by the state *before* a run and still be violated by the run itself: rsync
/// writes ~4.7 GB before pruning gets a chance to remove anything, because
/// pruning happens after the snapshot is taken.
///
/// 15 GB is now roughly three snapshots of headroom, which means the guard
/// survives the run that trips it rather than merely being true on entry.
const MIN_FREE_MB: u64 = 15_000;

/// Lock file preventing two snapshots from running at once.
const LOCK_FILE: &str = "/tmp/sys-snapshot.lock";

// ===========================================================================
//  EXCLUDE PATTERNS - READ THIS BEFORE EDITING
//
//  The single most important rsync rule this program depends on:
//
//    rsync matches exclude patterns RELATIVE TO THE TRANSFER ROOT, not as
//    absolute filesystem paths.
//
//  Concretely, for `rsync /home/alice/ dst/` the transfer root is /home/alice/,
//  so a file at /home/alice/.snapshots appears to rsync as "/.snapshots".
//
//  A pattern of "/home/alice/.snapshots" therefore matches NOTHING. The
//  leading slash means "anchor to the transfer root", so rsync is looking
//  for /home/alice/.snapshots relative to /home/alice/, which would be
//  /home/alice/home/alice/.snapshots. No such path exists.
//
//  The result of getting this wrong is silent and expensive: the snapshot
//  copies itself into itself, every snapshot is far larger than the last, and
//  the disk fills within a few hours. It cost one 2.4 GB snapshot to learn.
//
//  So: patterns WITHOUT a leading slash and WITH a trailing slash, such as
//  ".cache/", match a directory of that name at any depth. That is what we
//  want everywhere below.
// ===========================================================================

/// Build the exclude list at runtime, because the paths depend on $HOME.
fn excludes() -> Vec<String> {
    // Static patterns first, all of them by bare directory name.
    let list: Vec<&str> = vec![
        // The snapshot store itself. This lives inside the home directory
        // that we snapshot, so without this line the snapshot copies itself.
        ".snapshots/",
        // Caches: regenerated on demand, do not change between snapshots,
        // and together account for most of the home directory.
        ".cache/",
        "tool-output/",
        // Toolchains. The Rust toolchain alone is around 600 MB and comes
        // back with a single rustup command.
        ".rustup/",
        ".cargo/",
        // Trash and throwaway build output.
        "Trash/",
        // Privileged parts of /etc that a normal user simply cannot read.
        // rsync would emit "Permission denied" for these every single hour,
        // and there is nothing to copy anyway: these come back from their
        // packages. A leading slash is correct here, and means "directly
        // under /etc" rather than "anywhere", which is what we want.
        "/ssl/private/",
        "/sudoers.d/",
        "/shadow",
        "/gshadow",
        // Files the kernel regenerates at boot.
        "/mtab",
        "/resolv.conf",
        // Build output of every Rust project under $HOME. Cargo's target/
        // reaches 8+ GB per project and changes on EVERY build, so including
        // it both bloats the snapshot and destroys --link-dest sharing: each
        // hourly snapshot hardlinks a fresh copy of the whole build tree.
        //
        // The pattern must be the BARE NAME with a trailing slash, for the
        // reason spelled out above: $HOME/target does not exist, and an
        // absolute pattern matches nothing at all. "target/" matches any
        // directory called target at any depth, which is what we mean.
        "target/",
        // Same story for other build trees: node_modules is regenerated by
        // one npm install, and .git/objects dominates every source repo.
        // Wait — .git is NOT excluded: it is the user's history and the
        // cheapest thing to restore from a remote anyway. Only bulky
        // regenerable output belongs on this list.
        "node_modules/",
        ".venv/",
    ];

    // One allocation pass instead of calling to_string on every literal.
    // 📗 Теперь список статичен: ни один путь не зависит от $HOME. Раньше он
    //    зависел от домашнего каталога И был абсолютным, из-за чего не
    //    матчил ничего (см. правило rsync выше). Нет зависимости — нет ошибки.
    list.iter().map(|s| s.to_string()).collect()
}

// ===========================================================================
//  LOGGING
//
//  Diagnostics go to stdout and carry an emoji marker, because this output is
//  read by a human scanning for the one line that failed. Nothing is parsed
//  programmatically, so the formatting is free to be readable.
// ===========================================================================

/// Informational line: `[stage] message`.
fn step(stage: &str, msg: &str) {
    println!("[{stage}] {msg}");
}

/// Success line.
fn ok(msg: &str) {
    println!("  + {msg}");
}

/// Failure line.
fn err(msg: &str) {
    println!("  ! {msg}");
}

/// Plain line without a stage marker.
fn info(msg: &str) {
    println!("{msg}");
}

// ===========================================================================
//  HELPERS
// ===========================================================================

/// Snapshot identifier: UTC timestamp as YYYYMMDD-HHMMSS.
///
/// UTC rather than local time on purpose. A daylight saving change must not
/// reorder snapshots or push a new one into the past.
fn snapshot_id() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // Convert days since the epoch to a calendar date in a few lines rather
    // than pulling in a date library for this.
    let days = (secs / 86_400) as i64;
    let tod = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}{m:02}{d:02}-{:02}{:02}{:02}",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

/// Convert days since the Unix epoch to (year, month, day).
///
/// Howard Hinnant's algorithm. Correct for negative days too, which matters
/// because the usual "divide by 30" approximations are simply wrong.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // day of era
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day of year
    let mp = (5 * doy + 2) / 153; // month in a shifted 0..11 scale
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Free space on the filesystem holding `path`, in megabytes.
fn free_mb(path: &Path) -> u64 {
    // df -BM gives whole megabytes. A rough figure is fine here; precision to
    // the kilobyte would not change any decision made from it.
    Command::new("df")
        .arg("-BM")
        .arg(path)
        .output()
        .ok()
        .and_then(|o| {
            if !o.status.success() {
                return None;
            }
            let text = String::from_utf8_lossy(&o.stdout).to_string();
            // Header line then the data line. Column 4 is Available.
            text.lines()
                .nth(1)
                .and_then(|l| l.split_whitespace().nth(3))
                .and_then(|v| v.trim_end_matches('M').parse::<u64>().ok())
        })
        .unwrap_or(0)
}

/// Take the lock. Returns false when another snapshot is already running.
fn acquire_lock() -> bool {
    // create_new is atomic: if the file already exists, somebody else holds
    // the lock. No race, no timing assumption.
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(LOCK_FILE)
        .is_ok()
}

/// Release the lock.
fn release_lock() {
    let _ = fs::remove_file(LOCK_FILE);
}

/// Snapshots ordered oldest first.
fn list_snapshots() -> Vec<(String, PathBuf)> {
    let mut out: Vec<(String, PathBuf)> = Vec::new();

    // A missing store is not an error, it just means nothing has run yet.
    let entries = match fs::read_dir(snapshot_root()) {
        Ok(e) => e,
        Err(_) => return out,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            out.push((name.to_string(), path));
        }
    }

    // Names are fixed-width timestamps, so lexicographic order is also
    // chronological order.
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Size of the whole snapshot store in megabytes.
///
/// Measured across every snapshot at once on purpose: du deduplicates hard
/// links when it sees the files together, which is exactly the number we
/// care about, because a hard-linked file really is stored only once.
fn store_size_mb() -> String {
    Command::new("du")
        .arg("-smc")
        .arg(snapshot_root())
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|t| {
            t.lines()
                .next_back()
                .and_then(|l| l.split_whitespace().next())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| "?".into())
}

// ===========================================================================
//  TAKING A SNAPSHOT
// ===========================================================================

/// Snapshot each watched path, hard-linking unchanged files to the previous
/// snapshot, then prune old ones. Returns a process exit code.
fn take_snapshot() -> i32 {
    let root = snapshot_root();

    // Check free space BEFORE doing any work.
    let free = free_mb(&root);
    if free != 0 && free < MIN_FREE_MB {
        err(&format!(
            "{free} MB free, need at least {MIN_FREE_MB} MB - snapshot skipped"
        ));
        return 1;
    }
    if free != 0 {
        step("space", &format!("{free} MB free - enough"));
    }

    // Assert the recursion guard exists rather than trusting a comment.
    // If the pattern is ever removed from the exclude list, snapshots start
    // copying themselves into themselves, so this is the assertion that
    // actually matters here.
    let excl = excludes();
    if !excl.iter().any(|p| p == ".snapshots/") {
        err("the \".snapshots/\" exclude pattern is missing");
        err("snapshots would copy themselves; restore the pattern before running");
        return 1;
    }

    // A snapshot name is a timestamp with one-second resolution, so two runs
    // inside the same second would pick the same directory and the second run
    // would write into the first one's data. Worse, the second run would then
    // also find itself as its own --link-dest. Found by the integration test
    // on CI, where three quick runs collided.
    //
    // Appending a counter keeps every snapshot distinct and keeps the names
    // sortable, since "-2" sorts after the plain name.
    let mut id = snapshot_id();
    let mut dest = root.join(&id);
    let mut n = 2;
    while dest.exists() {
        id = format!("{}-{}", snapshot_id(), n);
        dest = root.join(&id);
        n += 1;
        if n > 1000 {
            err("cannot find an unused snapshot name");
            return 1;
        }
    }

    if let Err(e) = fs::create_dir_all(&dest) {
        err(&format!("cannot create {}: {e}", dest.display()));
        return 1;
    }
    step("snap", &format!("{id} -> {}", dest.display()));

    // Find the previous snapshot. rsync's --link-dest reuses files from it that
    // have not changed, which is what makes snapshots incremental: we pay
    // only for the difference.
    //
    // The destination must be excluded explicitly, because the directory was
    // already created above and so it is in this listing. Pointing --link-dest
    // at the empty directory we are about to fill makes every file copy
    // again instead of hard linking, which silently turns every snapshot
    // into a full copy. That regression was caught by the CI integration
    // test, which asserts shared inodes between consecutive snapshots.
    let existing = list_snapshots();
    let prev = existing
        .iter()
        .rev()
        .find(|(_, p)| p != &dest)
        .map(|(_, p)| p.clone());

    match &prev {
        Some(p) => step("link", &format!("hard-link base: {}", p.display())),
        None => step("link", "no previous snapshot - full copy this time"),
    }

    // Build the source list: configured system paths plus the home directory.
    let mut sources: Vec<(String, PathBuf)> = WATCH_SYSTEM
        .iter()
        .map(|(name, path)| (name.to_string(), PathBuf::from(path)))
        .collect();
    sources.push(("home".to_string(), home_dir()));

    let mut failed = false;

    for (name, source) in &sources {
        if !source.exists() {
            step(
                "skip",
                &format!("{name}: {} does not exist", source.display()),
            );
            continue;
        }

        let target = dest.join(name);

        // Run rsync through nice and ionice rather than directly.
        // nice -n 19  -> lowest CPU priority, invisible to interactive work
        // ionice -c3  -> idle IO class, disk only when the disk is otherwise idle
        // --bwlimit caps throughput so a snapshot cannot saturate the network
        let mut cmd = Command::new("nice");
        cmd.arg("-n").arg("19");
        cmd.arg("ionice").arg("-c").arg("3");
        cmd.arg("rsync");

        // -a preserves permissions, times, symlinks and device nodes.
        cmd.arg("-a");

        // --fake-super stores privileged attributes (uid/gid) in xattrs
        // rather than in the real ownership. Without it, root-owned files
        // from /etc land in the snapshot owned by root, and the snapshot
        // then cannot be deleted or pruned without sudo. With it, files
        // belong to the current user and "rsync --super" restores the real
        // owner at rollback time.
        cmd.arg("--fake-super");

        // --delete keeps a snapshot an exact copy instead of an accumulator
        // of everything that ever existed.
        cmd.arg("--delete");
        cmd.arg("--info=STATS2");

        if let Some(p) = &prev {
            cmd.arg(format!("--link-dest={}", p.join(name).display()));
        }

        for ex in &excl {
            cmd.arg(format!("--exclude={ex}"));
        }
        cmd.arg("--bwlimit=20000");

        cmd.arg(format!("{}/", source.display()));
        cmd.arg(&target);

        // Keep stdout clean, let rsync's own errors through to stderr.
        let out = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .output();

        match out {
            Ok(o) if o.status.success() => ok(&format!("{name}: captured")),

            // Exit code 23 means "some files could not be transferred". For a
            // non-root reader that is normal and expected: /etc/ssl/private
            // and /etc/shadow are unreadable by design. The snapshot is still
            // usable, it holds everything this account is meant to read.
            // Treating it as a failure would mark every snapshot as broken
            // because of access rights that are correct in the first place.
            Ok(o) if o.status.code() == Some(23) || o.status.code() == Some(24) => {
                step("partial", &format!(
                    "{name}: partially captured (exit {}) - some /etc files are not readable without root",
                    o.status.code().unwrap_or(0)
                ));
            }

            // Any other exit code is a genuine error.
            Ok(o) => {
                err(&format!("{name}: rsync exited with {}", o.status));
                failed = true;
            }
            Err(e) => {
                err(&format!("{name}: could not start - {e}"));
                failed = true;
            }
        }
    }

    if failed {
        err("snapshot is incomplete - rolling back to it may not be possible");
        return 1;
    }

    prune_old_snapshots();

    let count = list_snapshots().len();
    step(
        "size",
        &format!(
            "store total: {} MB across {count} snapshots",
            store_size_mb()
        ),
    );
    info("   (unchanged files are stored once and shared by every snapshot)");

    ok(&format!("snapshot {id} ready"));
    0
}

/// Delete snapshots beyond the retention limit.
fn prune_old_snapshots() {
    let all = list_snapshots();
    if all.len() <= KEEP_SNAPSHOTS {
        return;
    }

    let to_drop = all.len() - KEEP_SNAPSHOTS;
    for (name, path) in all.iter().take(to_drop) {
        // Make files writable before deleting. A snapshot may contain files
        // with restrictive modes, and remove_dir_all fails on those.
        // This only touches the owner's write bit and nothing else.
        let _ = Command::new("chmod")
            .arg("-R")
            .arg("u+w")
            .arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();

        match fs::remove_dir_all(path) {
            Ok(()) => step("prune", &format!("removed old snapshot {name}")),
            Err(e) => err(&format!("cannot remove {}: {e}", path.display())),
        }
    }
}

// ===========================================================================
//  LISTING
// ===========================================================================

/// Print the available snapshots and the space they use.
fn show_list() -> i32 {
    let all = list_snapshots();
    if all.is_empty() {
        info("No snapshots yet. Run: sys-snapshot");
        return 0;
    }

    info(&format!(
        "Snapshots: {} (keeping the most recent {KEEP_SNAPSHOTS})\n",
        all.len()
    ));

    // Measured across the whole store, not per snapshot: du does not
    // deduplicate hard links between separate arguments, so adding up
    // per-snapshot sizes would count every shared file many times over.
    info(&format!(
        "  TOTAL: {} MB  (shared files counted once)\n",
        store_size_mb()
    ));

    // Newest first, because that is the snapshot you are usually after.
    for (name, path) in all.iter().rev() {
        info(&format!("  {name}   {}", path.display()));
    }

    info("\nRollback: sys-snapshot --restore <ID>");
    0
}

// ===========================================================================
//  ROLLBACK
//
//  This deliberately does NOT perform the rollback and does NOT ask for
//  root. Restoring /etc blindly from a snapshot that turns out to be bad is
//  an excellent way to destroy a system. The point of this program is to
//  put the exact commands in front of a human who can read them first.
// ===========================================================================

/// Print rollback instructions for one snapshot.
fn show_restore(id: &str) -> i32 {
    let all = list_snapshots();
    let path = match all.iter().find(|(n, _)| n == id) {
        Some((_, p)) => p.clone(),
        None => {
            err(&format!("no snapshot named {id}"));
            info("   Available: sys-snapshot --list");
            return 1;
        }
    };

    let snap = path.display();
    info(&format!("Rollback to snapshot {id} - {}\n", snap));
    info("Nothing is applied automatically. Inspect first, then decide.");

    info("\n# 1. See what is actually in the snapshot:");
    info(&format!("ls -la {snap}/etc {snap}/home"));

    info("\n# 2. FIRST back up the current state, so the rollback is reversible:");
    // An earlier version wrote this backup inside the snapshot itself. That
    // is wrong twice over: it would modify the very snapshot used as the
    // restore point, and it would require root to write into a
    // user-owned directory. A separate directory in /tmp is unambiguous and
    // disposable.
    info("sudo mkdir -p /tmp/pre-rollback");
    info("sudo rsync -a /etc/ /tmp/pre-rollback/etc/");

    info("\n# 3. Restore a single file (safe, start here):");
    info(&format!(
        "sudo rsync -a --super {snap}/etc/fstab /etc/fstab"
    ));

    info("\n# 4. Restore a directory (overwrites current contents):");
    // --super is REQUIRED. Snapshots were taken with --fake-super, so the
    // owner lives in a user.rootless.* xattr while the file on disk belongs
    // to the user. A plain "rsync -a" ignores that xattr and recreates the
    // files owned by the user, which breaks a system directory. --super
    // tells rsync, running as root, to read and apply the saved owner.
    info(&format!("sudo rsync -a --super {snap}/etc/ /etc/"));
    info(&format!("sudo rsync -a --super {snap}/home/ /home/"));

    info("\n# 5. Reload if you touched systemd:");
    info("sudo systemctl daemon-reload && sudo reboot");

    info("\nNote: step 4 overwrites everything that changed SINCE the snapshot.");
    info("Undo the rollback: sudo rsync -a /tmp/pre-rollback/etc/ /etc/");
    0
}

// ===========================================================================
//  ENTRY POINT
// ===========================================================================

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();

    info("sys-snapshot - hourly snapshots with rollback");
    info(&format!(
        "   store: {}   keeping: {KEEP_SNAPSHOTS}",
        snapshot_root().display()
    ));

    // Nearly every mode needs the store directory to exist.
    if fs::create_dir_all(snapshot_root()).is_err() {
        err("cannot create the snapshot store");
        std::process::exit(1);
    }

    // Refuse to run twice at once: two snapshots writing the same directory
    // would corrupt each other's hard links.
    if !acquire_lock() {
        err("another snapshot is already running - wait for it");
        std::process::exit(1);
    }

    let code = match args.first().map(|s| s.as_str()) {
        Some("--list") => show_list(),
        Some("--restore") => match args.get(1) {
            Some(id) => show_restore(id),
            None => {
                err("usage: sys-snapshot --restore <ID>");
                1
            }
        },
        Some(other) => {
            err(&format!("unknown argument: {other}"));
            info("   sys-snapshot | --list | --restore <ID>");
            1
        }
        // No arguments means "take a snapshot now".
        None => take_snapshot(),
    };

    release_lock();
    std::process::exit(code);
}
