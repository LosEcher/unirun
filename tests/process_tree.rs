#![cfg(unix)]
//! The reference behaviour a deadline diverges from — kept in its own test
//! binary on purpose.
//!
//! `tests/matrix.rs` already holds nineteen acceptance cases, several of which
//! spawn processes and assert timing/drain behaviour. This case spawns too, and
//! it holds a child alive for over a second, which is exactly the window that
//! made `fa7a4ff` ("serialize process-spawning tests (fd race found by
//! release.sh)") necessary: a child forked while another *thread in the same
//! binary* sits between its pipe creation and its `spawn` can inherit that
//! pipe's write end, and then the owner's read never sees EOF. Cargo runs test
//! binaries one at a time, so a separate binary removes the window instead of
//! adding a lock to a file that has none.
//!
//! Recorded in `docs/PLATFORM-DIFFS.md` under "Process identity and lifetime".

use std::process::{Command, Stdio};
use std::time::Duration;

/// A sleep duration odd enough that `pgrep -f` cannot match anybody else's.
const TREE_MARKER: &str = "543.21";

fn tree_survivors() -> Vec<u32> {
    let out = Command::new("pgrep")
        .args(["-f", &format!("sleep {TREE_MARKER}")])
        .output()
        .expect("run pgrep");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect()
}

/// Never leave a nine-minute sleep behind, including on a failed assertion.
fn tree_sweep() {
    let _ = Command::new("pkill")
        .args(["-f", &format!("sleep {TREE_MARKER}")])
        .status();
}

/// A deadline kills the process **group**, not only the direct child.
///
/// `tests/matrix.rs::t_timeout_kills_whole_tree` infers the kill from the wall
/// clock (a survivor holds our stdout/stderr pipe open, so the run blocks). That
/// is a real detector but an indirect one, and it cannot show *why* the
/// mechanism matters. This case asserts the survivors directly and, in its first
/// arm, runs the reference behaviour — a PID-based killer that signals the
/// direct child only — so the divergence is demonstrated rather than asserted.
#[test]
fn t_tree_kill_diverges_from_killing_the_direct_child() {
    tree_sweep();

    // Reference arm: signal the direct child only.
    let mut naive = Command::new("sh")
        .arg("-c")
        .arg(format!("sleep {TREE_MARKER} & wait"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the reference arm");
    std::thread::sleep(Duration::from_millis(300));
    // SAFETY: `naive.id()` is our own live child.
    unsafe { libc::kill(naive.id() as i32, libc::SIGTERM) };
    let _ = naive.wait();
    std::thread::sleep(Duration::from_millis(300));
    let reference_survivors = tree_survivors();
    tree_sweep();

    // Subject arm: the same shape of command under a deadline.
    let r = unirun::run(&unirun::spec::ExecSpec {
        command: format!("sleep {TREE_MARKER} & sleep {TREE_MARKER}"),
        timeout_ms: 800,
        ..Default::default()
    });
    std::thread::sleep(Duration::from_millis(300));
    let our_survivors = tree_survivors();
    tree_sweep();

    assert!(r.timed_out, "expected a timeout, got {r:?}");
    assert!(
        !reference_survivors.is_empty(),
        "the reference arm left no survivor, so the divergence is not reproducible \
         as documented — check whether the shell now reaps its background jobs"
    );
    assert!(
        our_survivors.is_empty(),
        "the process tree survived the deadline kill: {our_survivors:?}"
    );
}
