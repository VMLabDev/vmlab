//! Windows backend: one recursive `ReadDirectoryChangesW` handle on the
//! watch root.
//!
//! There is nothing to prune here — a single handle covers the whole tree, so
//! the host's prune list has no registration to skip and is applied to the
//! reported names instead. That is the same list doing the same job at the
//! only point Windows offers.
//!
//! Windows reports the *entry* that changed, never its descendants, so the
//! backend keeps its own inventory of directories to tell the two cases
//! apart: a directory that appears has to be walked (its contents were never
//! reported), and a directory that is renamed away takes its children's
//! reports with it — which collapses to a rescan rather than leaving the host
//! to infer that a directory tombstone implies its children, an inference
//! §19.5 rules out.
//!
//! The read is synchronous and blocks until something changes; the channel's
//! canceller unblocks it with `CancelIoEx`.

use std::collections::HashSet;
use std::ffi::c_void;
use std::sync::Arc;
use std::thread;

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_OPERATION_ABORTED, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ACTION_ADDED, FILE_ACTION_REMOVED, FILE_ACTION_RENAMED_NEW_NAME,
    FILE_ACTION_RENAMED_OLD_NAME, FILE_ATTRIBUTE_DIRECTORY, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_ATTRIBUTES, FILE_NOTIFY_CHANGE_CREATION,
    FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE,
    FILE_NOTIFY_CHANGE_SECURITY, FILE_NOTIFY_CHANGE_SIZE, FILE_NOTIFY_INFORMATION,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileAttributesW,
    INVALID_FILE_ATTRIBUTES, OPEN_EXISTING, ReadDirectoryChangesW,
};
use windows_sys::Win32::System::IO::CancelIoEx;

use super::{Watch, join_rel};
use crate::windows::port::wide;

/// Everything the guest changes between two reads has to fit, or Windows
/// signals overflow instead. 256 KiB is the practical ceiling for a
/// non-local directory handle and plenty for a local one.
const BUFFER_BYTES: usize = 256 * 1024;

const FILTER: u32 = FILE_NOTIFY_CHANGE_FILE_NAME
    | FILE_NOTIFY_CHANGE_DIR_NAME
    | FILE_NOTIFY_CHANGE_ATTRIBUTES
    | FILE_NOTIFY_CHANGE_SIZE
    | FILE_NOTIFY_CHANGE_LAST_WRITE
    | FILE_NOTIFY_CHANGE_CREATION
    | FILE_NOTIFY_CHANGE_SECURITY;

/// The directory handle, shared with the canceller.
struct Dir(HANDLE);

// SAFETY: a HANDLE is a kernel object reference; ReadDirectoryChangesW and
// CancelIoEx on it from different threads is exactly the supported pattern.
unsafe impl Send for Dir {}
unsafe impl Sync for Dir {}

impl Drop for Dir {
    fn drop(&mut self) {
        // SAFETY: opened by `start`, closed exactly once here.
        unsafe { CloseHandle(self.0) };
    }
}

pub(super) fn start(watch: Arc<Watch>) -> Result<(), String> {
    let path = wide(&watch.root().to_string_lossy());
    // SAFETY: `path` is a NUL-terminated wide string that outlives the call.
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            FILE_LIST_DIRECTORY,
            // DELETE too: the root must be able to go away underneath us —
            // it is a channel failure to report, not one to prevent.
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let dir = Arc::new(Dir(handle));
    let cancel = dir.clone();
    watch.set_cancel(Box::new(move || {
        // SAFETY: the handle is alive while this Arc clone is.
        unsafe { CancelIoEx(cancel.0, std::ptr::null()) };
    }));

    let mut dirs = Inventory::default();
    dirs.walk(&watch, String::new(), false);
    thread::spawn(move || run(watch, dir, dirs));
    Ok(())
}

fn run(watch: Arc<Watch>, dir: Arc<Dir>, mut dirs: Inventory) {
    // u32-aligned, as FILE_NOTIFY_INFORMATION requires.
    let mut buffer = vec![0u32; BUFFER_BYTES / 4];
    while !watch.stopped() {
        let mut returned: u32 = 0;
        // SAFETY: the buffer outlives the call; the handle is alive while
        // `dir` is; a synchronous handle needs no OVERLAPPED.
        let ok = unsafe {
            ReadDirectoryChangesW(
                dir.0,
                buffer.as_mut_ptr() as *mut c_void,
                BUFFER_BYTES as u32,
                1, // watch the subtree
                FILTER,
                &mut returned,
                std::ptr::null_mut(),
                None,
            )
        };
        if watch.stopped() {
            return;
        }
        if ok == 0 {
            // SAFETY: no intervening Windows call since the failure.
            let err = unsafe { GetLastError() };
            if err == ERROR_OPERATION_ABORTED {
                return; // the channel closed under us
            }
            if !root_exists(&watch) {
                return watch.fail_root_gone();
            }
            return watch.fail(format!(
                "watch read: {}",
                std::io::Error::from_raw_os_error(err as i32)
            ));
        }
        if returned == 0 {
            // Too many changes to fit: whole-tree loss, exactly like a
            // queue overflow, and it collapses to the same value.
            watch.overflow();
            continue;
        }
        // SAFETY: the kernel filled `returned` bytes of `buffer` with a
        // chain of FILE_NOTIFY_INFORMATION records.
        for (action, name) in unsafe { records(&buffer, returned as usize) } {
            handle(&watch, &mut dirs, action, name);
        }
        if !root_exists(&watch) {
            return watch.fail_root_gone();
        }
    }
}

/// Fold one reported entry into the dirty set.
fn handle(watch: &Arc<Watch>, dirs: &mut Inventory, action: u32, rel: String) {
    watch.mark(rel.clone());
    if watch.is_pruned(&rel) {
        // A pruned directory is inert: never in the inventory, and never a
        // reason to rescan when it goes.
        return;
    }
    match action {
        FILE_ACTION_ADDED | FILE_ACTION_RENAMED_NEW_NAME if is_dir(watch, &rel) => {
            // A directory that appeared: nothing inside it was reported.
            dirs.insert(rel.clone());
            dirs.walk(watch, rel, true);
        }
        FILE_ACTION_REMOVED if dirs.remove_subtree(&rel) => {
            // A directory can only be removed once it is empty, so every
            // child was reported as it went.
        }
        FILE_ACTION_RENAMED_OLD_NAME if dirs.remove_subtree(&rel) => {
            // A directory renamed away reports nothing for its children.
            watch.overflow();
        }
        _ => {}
    }
}

/// Whether the watch root is still where the host named it. A renamed root
/// counts as gone: the handle follows the rename, but the workspace path the
/// host syncs no longer exists.
fn root_exists(watch: &Watch) -> bool {
    let path = wide(&watch.root().to_string_lossy());
    // SAFETY: `path` is a NUL-terminated wide string that outlives the call.
    let attrs = unsafe { GetFileAttributesW(path.as_ptr()) };
    attrs != INVALID_FILE_ATTRIBUTES && attrs & FILE_ATTRIBUTE_DIRECTORY != 0
}

fn is_dir(watch: &Watch, rel: &str) -> bool {
    watch
        .root()
        .join(rel)
        .symlink_metadata()
        .is_ok_and(|m| m.is_dir())
}

/// Which paths are directories, so a vanished one can be told from a file.
#[derive(Default)]
struct Inventory(HashSet<String>);

impl Inventory {
    fn insert(&mut self, rel: String) {
        self.0.insert(rel);
    }

    /// Record every directory at or below `start`, optionally marking every
    /// entry found on the way (what a subtree that appeared whole needs).
    fn walk(&mut self, watch: &Watch, start: String, mark: bool) {
        let mut stack = vec![start];
        while let Some(rel) = stack.pop() {
            if watch.is_pruned(&rel) {
                continue;
            }
            self.0.insert(rel.clone());
            let Ok(entries) = std::fs::read_dir(watch.root().join(&rel)) else {
                continue;
            };
            for entry in entries.flatten() {
                let child = join_rel(&rel, &entry.file_name().to_string_lossy());
                if mark {
                    watch.mark(child.clone());
                }
                // Symlinks are entries to report, never trees to descend.
                if entry.metadata().is_ok_and(|m| m.is_dir()) {
                    stack.push(child);
                }
            }
        }
    }

    /// Forget `rel` and everything below it. Returns whether `rel` was a
    /// directory this backend knew about.
    fn remove_subtree(&mut self, rel: &str) -> bool {
        let known = self.0.remove(rel);
        if known {
            self.0.retain(|p| !super::is_at_or_below(p, rel));
        }
        known
    }
}

/// Walk the FILE_NOTIFY_INFORMATION chain the kernel wrote into `buffer`,
/// yielding `(action, root-relative path)`.
///
/// # Safety
///
/// `buffer` must hold `len` bytes of a well-formed record chain.
unsafe fn records(buffer: &[u32], len: usize) -> Vec<(u32, String)> {
    let mut out = Vec::new();
    let base = buffer.as_ptr() as *const u8;
    let mut offset = 0usize;
    loop {
        if offset + size_of::<FILE_NOTIFY_INFORMATION>() > len {
            return out;
        }
        // SAFETY: the record starts within the filled region.
        let record = unsafe { &*(base.add(offset) as *const FILE_NOTIFY_INFORMATION) };
        let name_bytes = record.FileNameLength as usize;
        let name_at = offset + std::mem::offset_of!(FILE_NOTIFY_INFORMATION, FileName);
        if name_at + name_bytes > len {
            return out;
        }
        // SAFETY: the name is `FileNameLength` bytes of UTF-16 inside the
        // filled region; it is not NUL-terminated.
        let name =
            unsafe { std::slice::from_raw_parts(base.add(name_at) as *const u16, name_bytes / 2) };
        let rel = String::from_utf16_lossy(name).replace('\\', "/");
        if !rel.is_empty() {
            out.push((record.Action, rel));
        }
        match record.NextEntryOffset {
            0 => return out,
            next => offset += next as usize,
        }
    }
}
