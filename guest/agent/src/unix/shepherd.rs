//! Process trees on Linux (`features::TREE`): an exec that must stay
//! countable after its channel is closed.
//!
//! Closing an exec kills the process it started and nothing else, so an
//! installer a playbook step launched outlives the step's timeout. Its
//! parent is gone, so it is reparented, and a parent-child walk from the
//! killed process finds nothing. The shepherd closes that gap: it is a
//! re-exec of this binary (`--shepherd -- argv…`) that marks itself a child
//! subreaper and then starts `argv`. Every process `argv` starts, at any
//! depth, is reparented to the shepherd when its own parent dies, however it
//! detaches (`setsid`, a double fork, `su`). The shepherd reaps them and
//! exits once it has no children left, so the tree is alive exactly as long
//! as the shepherd is.
//!
//! The shepherd tells the agent two things on a status pipe at fd 3: its
//! own pid as soon as it starts, and the exit code of `argv` when that
//! process ends. The exec session ends on the second, while the shepherd may
//! go on reaping orphans. A record under [`trees_dir`] names the shepherd by
//! pid and start time, so a tree can be counted by a later agent process and
//! a recycled pid is never mistaken for it.
//!
//! In a container, a process inside the container's PID namespace whose
//! parent dies goes to the namespace's own init, which the shepherd cannot
//! be. The tree there counts the command and whatever it still parents.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::thread;

use nix::fcntl::OFlag;
use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};
use nix::unistd::pipe2;
use std::os::unix::process::CommandExt;

use super::{ExecPlan, spawn_piped_with};
use crate::spawn::Spawned;

/// The status pipe's fd in the shepherd.
const STATUS_FD: libc::c_int = 3;

/// The command the shepherd started, for the SIGTERM handler.
static COMMAND: AtomicI32 = AtomicI32::new(0);
/// A pidfd on the command, or -1: what the SIGTERM handler kills through,
/// so a SIGTERM racing the command's reaping can never reach a process that
/// was handed its pid afterwards.
static COMMAND_FD: AtomicI32 = AtomicI32::new(-1);
/// A SIGTERM that arrived before the command had a pid.
static TERMINATED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_sigterm(_: libc::c_int) {
    TERMINATED.store(true, Ordering::SeqCst);
    kill_command();
}

/// SIGKILL the command, through its pidfd where there is one.
fn kill_command() {
    let fd = COMMAND_FD.load(Ordering::SeqCst);
    if fd >= 0 {
        // SAFETY: pidfd_send_signal(2) is a plain, async-signal-safe syscall
        // on an fd this process holds open until it exits.
        unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                fd,
                libc::SIGKILL,
                std::ptr::null::<libc::siginfo_t>(),
                0u32,
            )
        };
        return;
    }
    let pid = COMMAND.load(Ordering::SeqCst);
    if pid > 0 {
        // SAFETY: kill(2) is async-signal-safe.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
}

/// `vmlab-agent --shepherd -- argv…`: become a child subreaper, start
/// `argv`, report its pid and exit code on fd 3, and reap until no child is
/// left. SIGTERM kills `argv` and nothing else, which is what closing the
/// exec does.
pub fn main(args: &[String]) -> ! {
    if args.first().map(String::as_str) != Some("--") || args.len() < 2 {
        eprintln!("vmlab-agent: bad --shepherd invocation");
        std::process::exit(127);
    }
    let argv = &args[1..];
    // SAFETY: fd 3 is the status pipe the agent dup'd in. Nothing the
    // command starts may hold it, or the agent would not see its end.
    let mut status = unsafe {
        libc::fcntl(STATUS_FD, libc::F_SETFD, libc::FD_CLOEXEC);
        File::from_raw_fd(STATUS_FD)
    };
    // SAFETY: plain prctl. A kernel without subreapers (before 3.4) still
    // runs the command; its orphans then go to init uncounted.
    unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) };
    let action = SigAction::new(
        SigHandler::Handler(on_sigterm),
        SaFlags::SA_RESTART,
        SigSet::empty(),
    );
    // SAFETY: the handler only touches atomics and calls kill(2).
    let _ = unsafe { sigaction(Signal::SIGTERM, &action) };
    let _ = writeln!(status, "{}", std::process::id());

    let mut command = Command::new(&argv[0]);
    command.args(&argv[1..]);
    // SAFETY: restores the default disposition in the child before exec;
    // signal(2) is async-signal-safe.
    unsafe {
        command.pre_exec(|| {
            libc::signal(libc::SIGTERM, libc::SIG_DFL);
            Ok(())
        })
    };
    let child = match command.spawn() {
        Ok(child) => child.id() as i32,
        Err(e) => {
            eprintln!("vmlab-agent: exec {}: {e}", argv[0]);
            let _ = writeln!(status, "127");
            std::process::exit(127);
        }
    };
    // SAFETY: pidfd_open(2) on the child just started, not yet reaped.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, child, 0u32) };
    if fd >= 0 {
        COMMAND_FD.store(fd as i32, Ordering::SeqCst);
    }
    COMMAND.store(child, Ordering::SeqCst);
    if TERMINATED.load(Ordering::SeqCst) {
        kill_command();
    }
    // The exec's stdio belong to the command. Holding them here would keep
    // the session's output open for as long as an orphan lives.
    release_stdio();

    let mut status = Some(status);
    loop {
        let mut raw = 0;
        // SAFETY: plain waitpid(2) for any child.
        let pid = unsafe { libc::waitpid(-1, &mut raw, 0) };
        if pid < 0 {
            match nix::errno::Errno::last() {
                nix::errno::Errno::EINTR => continue,
                _ => break, // ECHILD: the tree has drained
            }
        }
        if pid == child {
            // Reaped: its pid may be reused from here on, and only the
            // pidfd still names it.
            COMMAND.store(0, Ordering::SeqCst);
        }
        if pid == child
            && let Some(mut out) = status.take()
        {
            let code = if libc::WIFEXITED(raw) {
                libc::WEXITSTATUS(raw)
            } else if libc::WIFSIGNALED(raw) {
                128 + libc::WTERMSIG(raw)
            } else {
                127
            };
            let _ = writeln!(out, "{code}");
        }
    }
    std::process::exit(0);
}

/// Point fds 0-2 at /dev/null.
fn release_stdio() {
    let Ok(null) = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")
    else {
        return;
    };
    for fd in 0..=2 {
        // SAFETY: dup2 onto the standard fds; `null` stays open throughout.
        unsafe { libc::dup2(null.as_raw_fd(), fd) };
    }
}

/// Start `plan` under a shepherd and record it as `tree`. The returned
/// process's `kill` ends the command; its `wait` returns the command's exit
/// code without waiting for the rest of the tree.
pub(super) fn spawn(mut plan: ExecPlan, tree: &str) -> std::io::Result<Spawned> {
    let (status_r, status_w) = pipe2(OFlag::O_CLOEXEC)?;
    let command = std::mem::take(&mut plan.spec.argv);
    plan.spec.argv = shepherd_argv(command, &mut plan.spec.env);
    let w = status_w.as_raw_fd();
    let spawned = spawn_piped_with(plan, |cmd| {
        // SAFETY: dup2/fcntl between fork and exec, both async-signal-safe.
        // dup2 onto itself would keep the close-on-exec flag, so fd 3 is
        // cleared explicitly in that case.
        unsafe {
            cmd.pre_exec(move || {
                let rc = match w == STATUS_FD {
                    true => libc::fcntl(STATUS_FD, libc::F_SETFD, 0),
                    false => libc::dup2(w, STATUS_FD),
                };
                match rc < 0 {
                    true => Err(std::io::Error::last_os_error()),
                    false => Ok(()),
                }
            })
        };
    })?;
    drop(status_w);
    let mut status = BufReader::new(File::from(status_r));
    let Some(pid) = read_number::<u32>(&mut status) else {
        // The shepherd died before saying anything: no tree to record.
        let code = (spawned.wait)();
        return Err(std::io::Error::other(format!(
            "process tree {tree}: the shepherd exited with {code} before starting"
        )));
    };
    // Resolved before anything can reap the shepherd.
    let shepherd = super::ProcessRef::open(pid);
    record(tree, pid)?;
    let Spawned {
        input,
        output,
        errors,
        resize,
        kill: _,
        wait,
    } = spawned;
    let tree = tree.to_string();
    Ok(Spawned {
        input,
        output,
        errors,
        resize,
        kill: Box::new(move || {
            shepherd.signal(Signal::SIGTERM);
        }),
        wait: Box::new(move || match read_number::<i32>(&mut status) {
            Some(code) => {
                // The shepherd lingers while orphans live. Reap it, and
                // drop its record, when the last of them has gone.
                thread::spawn(move || {
                    wait();
                    forget(&tree, pid);
                });
                code
            }
            None => {
                let code = wait();
                forget(&tree, pid);
                code
            }
        }),
    })
}

/// This binary again, entered as the shepherd of `command`.
#[cfg(not(test))]
fn shepherd_argv(command: Vec<String>, _env: &mut Vec<(String, String)>) -> Vec<String> {
    let mut argv: Vec<String> = vec!["/proc/self/exe".into(), "--shepherd".into(), "--".into()];
    argv.extend(command);
    argv
}

/// The test binary again, entered through [`tests::shepherd_entry`]: a
/// libtest binary has no `--shepherd` of its own. The command travels in
/// the environment, because libtest would take any argument for a test
/// filter, and what it prints before the entry runs is set aside on fd 4
/// rather than mixed into the command's output.
#[cfg(test)]
fn shepherd_argv(command: Vec<String>, env: &mut Vec<(String, String)>) -> Vec<String> {
    env.push((
        TEST_ENTRY_VAR.into(),
        serde_json::to_string(&command).expect("an argv serialises"),
    ));
    let exe = std::env::current_exe().expect("the test binary's path");
    let mut argv: Vec<String> = ["/bin/sh", "-c", r#"exec 4>&1 >/dev/null; exec "$@""#, "sh"]
        .map(String::from)
        .to_vec();
    argv.push(exe.display().to_string());
    argv.extend(
        [
            "unix::shepherd::tests::shepherd_entry",
            "--exact",
            "--nocapture",
            "-q",
        ]
        .map(String::from),
    );
    argv
}

/// The command a test shepherd runs, as JSON.
#[cfg(test)]
const TEST_ENTRY_VAR: &str = "VMLAB_AGENT_TEST_SHEPHERD";

fn read_number<T: std::str::FromStr>(from: &mut impl BufRead) -> Option<T> {
    let mut line = String::new();
    from.read_line(&mut line).ok()?;
    line.trim().parse().ok()
}

/// Where tree records live: under `/run`, a tmpfs, so a guest restart
/// (which ends every tree) clears them. An agent that cannot write there
/// (not root, as under test) keeps them in the temp directory instead.
fn trees_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let run = PathBuf::from("/run/vmlab-agent/trees");
        match fs::create_dir_all(&run) {
            Ok(()) => run,
            Err(_) => std::env::temp_dir().join("vmlab-agent-trees"),
        }
    })
}

fn record(tree: &str, pid: u32) -> std::io::Result<()> {
    let dir = trees_dir();
    fs::create_dir_all(dir)?;
    let start = start_time(pid).ok_or_else(|| {
        std::io::Error::other(format!("process tree {tree}: shepherd {pid} vanished"))
    })?;
    fs::write(dir.join(tree), format!("{pid} {start}\n"))
}

/// Remove `tree`'s record if it still names `pid`: a later exec may have
/// reused the name.
fn forget(tree: &str, pid: u32) {
    let path = trees_dir().join(tree);
    if fs::read_to_string(&path)
        .ok()
        .and_then(|text| text.split_whitespace().next()?.parse::<u32>().ok())
        == Some(pid)
    {
        let _ = fs::remove_file(path);
    }
}

/// How many processes of `tree` are alive: every descendant of its
/// shepherd. Zero when there is no record, or the pid it names is no longer
/// that shepherd.
pub fn status(tree: &str) -> Result<u32, String> {
    match shepherd_of(tree) {
        Some(pid) => Ok(descendants(pid, &parents()?)),
        None => Ok(0),
    }
}

/// The live shepherd `tree`'s record names, dropping a stale record.
fn shepherd_of(tree: &str) -> Option<u32> {
    let path = trees_dir().join(tree);
    let text = fs::read_to_string(&path).ok()?;
    let mut fields = text.split_whitespace().map(str::parse::<u64>);
    let (Some(Ok(pid)), Some(Ok(start))) = (fields.next(), fields.next()) else {
        let _ = fs::remove_file(&path);
        return None;
    };
    let pid = pid as u32;
    if start_time(pid) != Some(start) {
        let _ = fs::remove_file(&path);
        return None;
    }
    Some(pid)
}

/// SIGKILL every process of `tree` below its shepherd, at any depth,
/// repeating while a pass still finds one so a process forked mid-pass is
/// caught too. The shepherd itself is left to its own kill, which ends the
/// command and lets it report the exit code.
///
/// This reaches what an environment scan cannot: a process that cleared its
/// environment, or detached with `setsid` and a double fork, is still the
/// shepherd's (PR_SET_CHILD_SUBREAPER).
pub fn kill_tree(tree: &str) {
    let Some(shepherd) = shepherd_of(tree) else {
        return;
    };
    for _ in 0..5 {
        let Ok(parents) = parents() else {
            return;
        };
        let members = members(shepherd, &parents);
        if members.is_empty() {
            return;
        }
        for pid in &members {
            let process = super::ProcessRef::open(*pid);
            // The pid was read from a scan: only kill it if, now that it is
            // pinned, it is still a member and not a newcomer to the number.
            let still_member =
                stat(*pid).is_some_and(|(ppid, _)| ppid == shepherd || members.contains(&ppid));
            if still_member {
                process.signal(Signal::SIGKILL);
            }
        }
        thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// One process's (parent, start time) from `/proc/<pid>/stat`.
fn stat(pid: u32) -> Option<(u32, u64)> {
    let text = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat_fields(&text)
}

/// Parse a `/proc/<pid>/stat` line. The command name is parenthesised and
/// may itself hold spaces and parentheses, so fields are counted from the
/// last `)`: state, then ppid, and start time is the 20th after it.
fn stat_fields(text: &str) -> Option<(u32, u64)> {
    let rest = &text[text.rfind(')')? + 1..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    Some((fields.get(1)?.parse().ok()?, fields.get(19)?.parse().ok()?))
}

fn start_time(pid: u32) -> Option<u64> {
    stat(pid).map(|(_, start)| start)
}

/// Every live process's parent.
fn parents() -> Result<HashMap<u32, u32>, String> {
    let entries = fs::read_dir("/proc").map_err(|e| format!("read /proc: {e}"))?;
    Ok(entries
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter_map(|pid| Some((pid, stat(pid)?.0)))
        .collect())
}

/// How many processes descend from `root` in `parents`.
fn descendants(root: u32, parents: &HashMap<u32, u32>) -> u32 {
    members(root, parents).len() as u32
}

/// Every process descending from `root` in `parents`, at any depth.
fn members(root: u32, parents: &HashMap<u32, u32>) -> Vec<u32> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for (&pid, &ppid) in parents {
        children.entry(ppid).or_default().push(pid);
    }
    let mut found = Vec::new();
    let mut stack = vec![root];
    while let Some(pid) = stack.pop() {
        for &child in children.get(&pid).map(Vec::as_slice).unwrap_or_default() {
            found.push(child);
            stack.push(child);
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shepherd, when the test binary is re-entered as one (see
    /// [`shepherd_argv`]). Run as an ordinary test it does nothing.
    #[test]
    fn shepherd_entry() {
        let Some(command) = std::env::var(TEST_ENTRY_VAR)
            .ok()
            .and_then(|json| serde_json::from_str::<Vec<String>>(&json).ok())
        else {
            return;
        };
        // The command must not see itself as a shepherd's.
        // SAFETY: the process is about to become the shepherd; no other
        // thread of this test binary reads the environment.
        unsafe { std::env::remove_var(TEST_ENTRY_VAR) };
        // SAFETY: restoring the stdout `shepherd_argv` set aside on fd 4.
        unsafe {
            libc::dup2(4, 1);
            libc::close(4);
        }
        let mut args = vec!["--".to_string()];
        args.extend(command);
        main(&args);
    }

    #[test]
    fn stat_fields_survive_a_command_name_with_spaces_and_parens() {
        let line = "4242 (my (odd) cmd) S 17 4242 4242 0 -1 4194560 100 0 0 0 1 2 0 0 \
                    20 0 1 0 987654 1234 56 18446744073709551615";
        assert_eq!(stat_fields(line), Some((17, 987654)));
        assert_eq!(stat_fields("garbage"), None);
    }

    #[test]
    fn descendants_count_every_depth_and_nothing_beside() {
        // 10 → 11 → 12, 10 → 13; 20 is unrelated, 1 is everyone's init.
        let parents = HashMap::from([(10, 1), (11, 10), (12, 11), (13, 10), (20, 1)]);
        assert_eq!(descendants(10, &parents), 3);
        assert_eq!(descendants(12, &parents), 0);
    }
}
