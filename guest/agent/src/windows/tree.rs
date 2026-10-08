//! Process trees (`features::TREE`) as named job objects.
//!
//! An exec opened inside a tree runs in the job `Local\vmlab-tree-<name>`,
//! and so does everything it starts: a job's membership is inherited by
//! every child `CreateProcess` makes unless the job allows breakaway, which
//! these never do. Closing the channel still kills only the direct process;
//! what this buys is that its orphans stay *countable*, so the host can
//! refuse to start the next run while an installer the last one launched is
//! still going.
//!
//! The job is created with no limits at all — in particular not
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`: the members are meant to survive
//! the agent letting go of the job, and to be counted, not killed.
//!
//! **Why the agent keeps a handle in [`JOBS`].** Microsoft documents that
//! "the job is destroyed when its last handle has been closed and all
//! associated processes have exited" (`CreateJobObjectW`, Remarks), so the
//! object outlives our handle while a member runs — but its *name* does not.
//! A process holds a reference to its job, not a handle, and the object
//! manager drops a temporary object's name from the namespace as soon as its
//! handle count reaches zero. With no handle open, `OpenJobObjectW` would
//! answer `ERROR_FILE_NOT_FOUND` for a job that still has a live member, and
//! [`status`] would report a running installer as zero. So the agent holds
//! one handle per tree here, and lets go only once the job has been seen
//! empty: after an exec in it is reaped, or when [`status`] counts zero.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};

use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, HANDLE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
    JobObjectBasicAccountingInformation, OpenJobObjectW, QueryInformationJobObject,
};
use windows_sys::Win32::System::SystemServices::JOB_OBJECT_QUERY;

use super::port::wide;
use super::proc::Owned;

/// One handle per tree that may still have members, keyed by tree name.
///
/// The lock is also what makes "empty" mean empty: a spawn holds it from
/// finding the job until its process is assigned, so a status query can
/// never see the gap between the two, count zero, and close the handle the
/// spawn is about to assign through.
static JOBS: Mutex<BTreeMap<String, Owned>> = Mutex::new(BTreeMap::new());

fn jobs() -> MutexGuard<'static, BTreeMap<String, Owned>> {
    JOBS.lock().unwrap_or_else(|e| e.into_inner())
}

/// The job's object name. `Local\` because the agent is the only party that
/// creates or opens it, always from the same session.
fn object_name(tree: &str) -> Vec<u16> {
    wide(&format!("Local\\vmlab-tree-{tree}"))
}

/// A tree's job, found or created, with the registry locked until the new
/// process has been assigned (see [`JOBS`]).
pub struct Joining {
    jobs: MutexGuard<'static, BTreeMap<String, Owned>>,
    tree: String,
    job: HANDLE,
}

/// Find or create the job for `tree`, ready to take a process.
pub fn join(tree: &str) -> std::io::Result<Joining> {
    let mut jobs = jobs();
    let job = match jobs.get(tree) {
        Some(job) => job.0,
        None => {
            let name = object_name(tree);
            // SAFETY: a null security descriptor and a NUL-terminated name
            // that lives across the call. An existing job of this name is
            // opened rather than replaced, which is what a tree reused after
            // an agent restart wants.
            let job = unsafe { CreateJobObjectW(std::ptr::null(), name.as_ptr()) };
            if job.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            jobs.insert(tree.to_string(), Owned(job));
            job
        }
    };
    Ok(Joining {
        jobs,
        tree: tree.to_string(),
        job,
    })
}

impl Joining {
    /// Put `process` — created suspended, so it has run nothing yet — in the
    /// job. Anything it starts from here on is a member too.
    pub fn assign(&self, process: HANDLE) -> std::io::Result<()> {
        // SAFETY: the job handle is held by the locked registry; the process
        // handle is the caller's, live and carrying PROCESS_SET_QUOTA |
        // PROCESS_TERMINATE (a handle from CreateProcess has all access).
        if unsafe { AssignProcessToJobObject(self.job, process) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    /// The spawn failed: drop the job again if nothing ended up in it.
    pub fn abandon(mut self) {
        if active(self.job).is_ok_and(|n| n == 0) {
            self.jobs.remove(&self.tree);
        }
    }
}

/// How many processes are in the job right now.
fn active(job: HANDLE) -> std::io::Result<u32> {
    let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
    // SAFETY: `info` is the struct this class fills, sized to match, and
    // lives across the call; `job` carries at least JOB_OBJECT_QUERY.
    let ok = unsafe {
        QueryInformationJobObject(
            job,
            JobObjectBasicAccountingInformation,
            &mut info as *mut _ as *mut core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(info.ActiveProcesses)
}

/// Let go of `tree`'s job if it has no members left — called once an exec
/// in it has been reaped, so a tree whose run left nothing behind does not
/// hold a handle until the host happens to ask.
pub fn release_if_empty(tree: &str) {
    let mut jobs = jobs();
    let empty = jobs
        .get(tree)
        .is_some_and(|job| active(job.0).is_ok_and(|n| n == 0));
    if empty {
        jobs.remove(tree);
    }
}

/// How many processes of `tree` are still alive.
///
/// A tree with no job is a tree with nothing in it: never opened on this
/// agent, or already seen empty and released. Counting zero releases the
/// agent's handle, which lets the name go.
pub fn status(tree: &str) -> Result<u32, String> {
    let mut jobs = jobs();
    let name = object_name(tree);
    // SAFETY: a NUL-terminated name that lives across the call.
    let job = unsafe { OpenJobObjectW(JOB_OBJECT_QUERY, 0, name.as_ptr()) };
    if job.is_null() {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32) {
            jobs.remove(tree);
            return Ok(0);
        }
        return Err(format!("open job: {e}"));
    }
    let job = Owned(job);
    let alive = active(job.0).map_err(|e| format!("query job: {e}"))?;
    if alive == 0 {
        jobs.remove(tree);
    }
    Ok(alive)
}
