#![cfg(windows)]
use super::*;
use ntapi::ntpebteb::PEB;
use ntapi::ntpsapi::{
    NtQueryInformationProcess, ProcessBasicInformation, ProcessSequenceNumber,
    ProcessWow64Information, PROCESS_BASIC_INFORMATION,
};
use ntapi::ntrtl::RTL_USER_PROCESS_PARAMETERS;
use ntapi::ntwow64::RTL_USER_PROCESS_PARAMETERS32;
use std::ffi::OsString;
use std::mem::MaybeUninit;
use std::os::windows::ffi::OsStringExt;
use winapi::shared::minwindef::{DWORD, FILETIME, LPVOID, MAX_PATH};
use winapi::shared::ntdef::{FALSE, NT_SUCCESS};
use winapi::um::handleapi::CloseHandle;
use winapi::um::memoryapi::ReadProcessMemory;
use winapi::um::processthreadsapi::{GetCurrentProcessId, GetProcessTimes, OpenProcess};
use winapi::um::shellapi::CommandLineToArgvW;
use winapi::um::tlhelp32::*;
use winapi::um::winbase::{LocalFree, QueryFullProcessImageNameW};
use winapi::um::winnt::{
    HANDLE, PROCESS_QUERY_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
};

/// Manages a Toolhelp32 snapshot handle
struct Snapshot(HANDLE);

impl Snapshot {
    pub fn new() -> Option<Self> {
        let handle = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if handle.is_null() {
            None
        } else {
            Some(Self(handle))
        }
    }

    pub fn iter(&self) -> ProcIter {
        ProcIter {
            snapshot: &self,
            first: true,
        }
    }

    pub fn entries() -> Vec<PROCESSENTRY32W> {
        match Self::new() {
            Some(snapshot) => snapshot.iter().collect(),
            None => vec![],
        }
    }
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

struct ProcIter<'a> {
    snapshot: &'a Snapshot,
    first: bool,
}

impl<'a> Iterator for ProcIter<'a> {
    type Item = PROCESSENTRY32W;

    fn next(&mut self) -> Option<Self::Item> {
        let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as _;
        let res = if self.first {
            self.first = false;
            unsafe { Process32FirstW(self.snapshot.0, &mut entry) }
        } else {
            unsafe { Process32NextW(self.snapshot.0, &mut entry) }
        };
        if res == 0 {
            None
        } else {
            Some(entry)
        }
    }
}

fn wstr_to_path(slice: &[u16]) -> PathBuf {
    match slice.iter().position(|&c| c == 0) {
        Some(nul) => OsString::from_wide(&slice[..nul]),
        None => OsString::from_wide(slice),
    }
    .into()
}

fn wstr_to_string(slice: &[u16]) -> String {
    wstr_to_path(slice).to_string_lossy().into_owned()
}

struct ProcParams {
    argv: Vec<String>,
    cwd: PathBuf,
    console: HANDLE,
}

/// A handle to an opened process
struct ProcHandle {
    pid: u32,
    proc: HANDLE,
}

impl ProcHandle {
    pub fn new(pid: u32) -> Option<Self> {
        if pid == unsafe { GetCurrentProcessId() } {
            // Avoid the potential for deadlock if we're examining ourselves
            log::trace!("ProcHandle::new({}): skip because it is my own pid", pid);
            return None;
        }
        let options = PROCESS_QUERY_INFORMATION | PROCESS_VM_READ;
        log::trace!("ProcHandle::new({}): OpenProcess", pid);
        let handle = unsafe { OpenProcess(options, FALSE as _, pid) };
        log::trace!("ProcHandle::new({}): OpenProcess -> {:?}", pid, handle);
        if handle.is_null() {
            return None;
        }
        Some(Self { pid, proc: handle })
    }

    /// A handle that can only be queried, which Windows grants for elevated
    /// and protected processes as well. It reads nothing from the process,
    /// so our own pid is fine here.
    fn query_only(pid: u32) -> Option<Self> {
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE as _, pid) };
        if handle.is_null() {
            return None;
        }
        Some(Self { pid, proc: handle })
    }

    /// Returns the executable image for the process
    pub fn executable(&self) -> Option<PathBuf> {
        let mut buf = [0u16; MAX_PATH + 1];
        let mut len = buf.len() as DWORD;
        let res = unsafe { QueryFullProcessImageNameW(self.proc, 0, buf.as_mut_ptr(), &mut len) };
        if res == 0 {
            None
        } else {
            Some(wstr_to_path(&buf))
        }
    }

    /// Wrapper around NtQueryInformationProcess that fetches `what` as `T`
    fn query_proc<T>(&self, what: u32) -> Option<T> {
        let mut data = MaybeUninit::<T>::uninit();
        let res = unsafe {
            NtQueryInformationProcess(
                self.proc,
                what,
                data.as_mut_ptr() as _,
                std::mem::size_of::<T>() as _,
                std::ptr::null_mut(),
            )
        };
        if !NT_SUCCESS(res) {
            return None;
        }
        let data = unsafe { data.assume_init() };
        Some(data)
    }

    /// Read a `T` from the target process at the specified address
    fn read_struct<T>(&self, addr: LPVOID) -> Option<T> {
        let mut data = MaybeUninit::<T>::uninit();
        let res = unsafe {
            ReadProcessMemory(
                self.proc,
                addr as _,
                data.as_mut_ptr() as _,
                std::mem::size_of::<T>() as _,
                std::ptr::null_mut(),
            )
        };
        if res == 0 {
            return None;
        }
        let data = unsafe { data.assume_init() };
        Some(data)
    }

    /// If the process is a 32-bit process running on Win64, return the address
    /// of its process parameters.
    /// Otherwise, return None to indicate a native win64 process.
    fn get_peb32_addr(&self) -> Option<LPVOID> {
        let peb32_addr: LPVOID = self.query_proc(ProcessWow64Information)?;
        if peb32_addr.is_null() {
            None
        } else {
            Some(peb32_addr)
        }
    }

    /// Returns the cwd and args for the process
    pub fn get_params(&self) -> Option<ProcParams> {
        match self.get_peb32_addr() {
            Some(peb32) => self.get_params_32(peb32),
            None => self.get_params_64(),
        }
    }

    fn get_basic_info(&self) -> Option<PROCESS_BASIC_INFORMATION> {
        self.query_proc(ProcessBasicInformation)
    }

    fn get_peb(&self, info: &PROCESS_BASIC_INFORMATION) -> Option<PEB> {
        self.read_struct(info.PebBaseAddress as _)
    }

    fn get_proc_params(&self, peb: &PEB) -> Option<RTL_USER_PROCESS_PARAMETERS> {
        self.read_struct(peb.ProcessParameters as _)
    }

    /// Returns the cwd and args for a 64 bit process
    fn get_params_64(&self) -> Option<ProcParams> {
        let info = self.get_basic_info()?;
        let peb = self.get_peb(&info)?;
        let params = self.get_proc_params(&peb)?;

        let cmdline = self.read_process_wchar(
            params.CommandLine.Buffer as _,
            params.CommandLine.Length as _,
        )?;
        let cwd = self.read_process_wchar(
            params.CurrentDirectory.DosPath.Buffer as _,
            params.CurrentDirectory.DosPath.Length as _,
        )?;

        Some(ProcParams {
            argv: cmd_line_to_argv(&cmdline),
            cwd: wstr_to_path(&cwd),
            console: params.ConsoleHandle,
        })
    }

    fn get_proc_params_32(&self, peb32: LPVOID) -> Option<RTL_USER_PROCESS_PARAMETERS32> {
        self.read_struct(peb32)
    }

    /// Returns the cwd and args for a 32 bit process
    fn get_params_32(&self, peb32: LPVOID) -> Option<ProcParams> {
        let params = self.get_proc_params_32(peb32)?;

        let cmdline = self.read_process_wchar(
            params.CommandLine.Buffer as _,
            params.CommandLine.Length as _,
        )?;
        let cwd = self.read_process_wchar(
            params.CurrentDirectory.DosPath.Buffer as _,
            params.CurrentDirectory.DosPath.Length as _,
        )?;

        Some(ProcParams {
            argv: cmd_line_to_argv(&cmdline),
            cwd: wstr_to_path(&cwd),
            console: params.ConsoleHandle as _,
        })
    }

    /// Copies a sized WSTR from the address in the process
    fn read_process_wchar(&self, ptr: LPVOID, byte_size: usize) -> Option<Vec<u16>> {
        if byte_size > MAX_PATH * 4 {
            // Defend against implausibly large paths, just in
            // case we're reading the wrong offset into a kernel struct
            return None;
        }

        let mut buf = vec![0u16; byte_size / 2];
        let mut bytes_read = 0;

        let res = unsafe {
            ReadProcessMemory(
                self.proc,
                ptr as _,
                buf.as_mut_ptr() as _,
                byte_size,
                &mut bytes_read,
            )
        };
        if res == 0 {
            return None;
        }

        // In the unlikely event that we have a short read,
        // truncate the buffer to fit.
        let wide_chars_read = bytes_read / 2;
        buf.resize(wide_chars_read, 0);

        // Ensure that it is NUL terminated
        match buf.iter().position(|&c| c == 0) {
            Some(n) => {
                // Truncate to include existing NUL but no later chars
                buf.resize(n + 1, 0);
            }
            None => {
                // Add a NUL
                buf.push(0);
            }
        }

        Some(buf)
    }

    /// Retrieves the start time of the process
    fn start_time(&self) -> Option<u64> {
        const fn empty() -> FILETIME {
            FILETIME {
                dwLowDateTime: 0,
                dwHighDateTime: 0,
            }
        }

        let mut start = empty();
        let mut exit = empty();
        let mut kernel = empty();
        let mut user = empty();

        let res =
            unsafe { GetProcessTimes(self.proc, &mut start, &mut exit, &mut kernel, &mut user) };
        if res == 0 {
            return None;
        }

        Some((start.dwHighDateTime as u64) << 32 | start.dwLowDateTime as u64)
    }

    /// Where the process stands in creation order. Both answers need only a
    /// `query_only` handle.
    fn birth(&self) -> Birth {
        Birth {
            sequence: self.query_proc(ProcessSequenceNumber),
            time: self.start_time(),
        }
    }
}

/// Where a process stands in creation order, which tells a real parent from
/// a younger process that was handed the parent's pid after it exited.
#[derive(Clone, Copy, Default)]
struct Birth {
    /// Counts the processes created since boot. Unlike the creation time it
    /// does not run backwards when the system clock is stepped back.
    sequence: Option<u64>,
    /// Creation time in FILETIME units, for when there is no sequence number.
    time: Option<u64>,
}

impl Birth {
    fn read(pid: u32) -> Self {
        ProcHandle::query_only(pid)
            .map(|proc| proc.birth())
            .unwrap_or_default()
    }

    /// Whether this process is known to have started before `other`.
    fn before(&self, other: &Birth) -> bool {
        match (self.sequence, other.sequence) {
            (Some(mine), Some(theirs)) => mine < theirs,
            _ => matches!((self.time, other.time), (Some(mine), Some(theirs)) if mine < theirs),
        }
    }

    /// What this process's children are measured against: this process
    /// where it is known, the nearest ancestor that is known otherwise, as a
    /// process starts after every one of its ancestors.
    fn or(self, ancestor: Birth) -> Birth {
        Birth {
            sequence: self.sequence.or(ancestor.sequence),
            time: self.time.or(ancestor.time),
        }
    }
}

/// What `with_root_pid` records about one process besides its table entry
/// and its birth.
#[derive(Default)]
struct ProcFacts {
    executable: Option<PathBuf>,
    cwd: PathBuf,
    argv: Vec<String>,
    console: u64,
}

impl ProcFacts {
    fn read(pid: u32) -> Self {
        let mut facts = Self::default();
        if let Some(proc) = ProcHandle::new(pid) {
            facts.executable = proc.executable();
            if let Some(params) = proc.get_params() {
                facts.cwd = params.cwd;
                facts.argv = params.argv;
                facts.console = params.console as _;
            }
        }
        facts
    }
}

/// Where `build_tree` learns about a process beyond its table entry; the
/// tests stand in for the live system here.
trait ProcSource {
    fn birth(&mut self, pid: u32) -> Birth;
    fn facts(&mut self, pid: u32) -> ProcFacts;
}

struct LiveProcs;

impl ProcSource for LiveProcs {
    fn birth(&mut self, pid: u32) -> Birth {
        Birth::read(pid)
    }

    fn facts(&mut self, pid: u32) -> ProcFacts {
        ProcFacts::read(pid)
    }
}

/// Deeper than any real process tree. A runaway chain, a script that keeps
/// starting itself, is cut here instead of being walked until the stack
/// runs out: the mux server walks pane trees on its 1 MiB main thread.
const MAX_DEPTH: usize = 64;

/// The tree below `info`, which must not have started before `bound`: its
/// nearest known ancestor.
///
/// Windows records the pid of a process's creator once and hands the pid
/// out again after that process exits, so the parent a snapshot names can
/// be a younger, unrelated process -- even one of the child's own
/// descendants. A daemon whose launcher has exited, as the mux server's
/// has, becomes the child of whichever program in one of its own panes is
/// given the launcher's pid, and following children then goes round and
/// round until the stack overflows. A process that started before its
/// ancestor is not below it; `visited` ends any loop the births cannot rule
/// out.
fn build_tree(
    info: &PROCESSENTRY32W,
    procs: &[PROCESSENTRY32W],
    bound: Birth,
    depth: usize,
    visited: &mut HashSet<u32>,
    source: &mut dyn ProcSource,
) -> Option<LocalProcessInfo> {
    if !visited.insert(info.th32ProcessID) {
        return None;
    }
    let birth = source.birth(info.th32ProcessID);
    if birth.before(&bound) {
        return None;
    }
    let facts = source.facts(info.th32ProcessID);

    let mut children = HashMap::new();
    if depth < MAX_DEPTH {
        let bound = birth.or(bound);
        for kid in procs {
            if kid.th32ParentProcessID == info.th32ProcessID {
                if let Some(child) = build_tree(kid, procs, bound, depth + 1, visited, source) {
                    children.insert(kid.th32ProcessID, child);
                }
            }
        }
    }

    let executable = facts
        .executable
        .unwrap_or_else(|| wstr_to_path(&info.szExeFile));
    let name = match executable.file_name() {
        Some(name) => name.to_string_lossy().into_owned(),
        None => String::new(),
    };

    Some(LocalProcessInfo {
        pid: info.th32ProcessID,
        ppid: info.th32ParentProcessID,
        name,
        executable,
        cwd: facts.cwd,
        argv: facts.argv,
        start_time: birth.time.unwrap_or(0),
        status: LocalProcessStatus::Run,
        children,
        console: facts.console,
    })
}

/// Parse a command line string into an argv array
fn cmd_line_to_argv(buf: &[u16]) -> Vec<String> {
    let mut argc = 0;
    let argvp = unsafe { CommandLineToArgvW(buf.as_ptr(), &mut argc) };
    if argvp.is_null() {
        return vec![];
    }

    let argv = unsafe { std::slice::from_raw_parts(argvp, argc as usize) };
    let mut args = vec![];
    for &arg in argv {
        let len = unsafe { libc::wcslen(arg) };
        let arg = unsafe { std::slice::from_raw_parts(arg, len) };
        args.push(wstr_to_string(arg));
    }
    unsafe { LocalFree(argvp as _) };
    args
}

impl Drop for ProcHandle {
    fn drop(&mut self) {
        log::trace!("ProcHandle::drop(pid={} proc={:?})", self.pid, self.proc);
        unsafe { CloseHandle(self.proc) };
    }
}

/// See [`crate::ProcessTableEntry`]: one Toolhelp32 pass, no per-process
/// handles.
pub fn process_table() -> Option<Vec<crate::ProcessTableEntry>> {
    let entries = Snapshot::entries();
    if entries.is_empty() {
        return None;
    }
    Some(
        entries
            .iter()
            .map(|entry| {
                let len = entry
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szExeFile.len());
                crate::ProcessTableEntry {
                    pid: entry.th32ProcessID,
                    ppid: entry.th32ParentProcessID,
                    name: String::from_utf16_lossy(&entry.szExeFile[..len]),
                }
            })
            .collect(),
    )
}

impl LocalProcessInfo {
    pub fn current_working_dir(pid: u32) -> Option<PathBuf> {
        log::trace!("current_working_dir({})", pid);
        let proc = ProcHandle::new(pid)?;
        let params = proc.get_params()?;
        Some(params.cwd)
    }

    pub fn executable_path(pid: u32) -> Option<PathBuf> {
        log::trace!("executable_path({})", pid);
        let proc = ProcHandle::new(pid)?;
        proc.executable()
    }

    /// The argument vector of one process, read from its PEB. `None` when
    /// the process is gone or its parameters are unreadable.
    pub fn argv_for_pid(pid: u32) -> Option<Vec<String>> {
        log::trace!("argv_for_pid({})", pid);
        let params = ProcHandle::new(pid)?.get_params()?;
        if params.argv.is_empty() {
            return None;
        }
        Some(params.argv)
    }

    pub fn with_root_pid(pid: u32) -> Option<Self> {
        log::trace!("LocalProcessInfo::with_root_pid({}), getting snapshot", pid);
        let procs = Snapshot::entries();
        log::trace!("Got snapshot");

        let info = procs.iter().find(|info| info.th32ProcessID == pid)?;
        build_tree(
            info,
            &procs,
            Birth::default(),
            0,
            &mut HashSet::new(),
            &mut LiveProcs,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(pid: u32, ppid: u32) -> PROCESSENTRY32W {
        let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as _;
        entry.th32ProcessID = pid;
        entry.th32ParentProcessID = ppid;
        entry
    }

    /// Births by pid; a pid without one is a process that could not be
    /// opened or has exited since the snapshot.
    struct Births(HashMap<u32, Birth>);

    impl ProcSource for Births {
        fn birth(&mut self, pid: u32) -> Birth {
            self.0.get(&pid).copied().unwrap_or_default()
        }

        fn facts(&mut self, _pid: u32) -> ProcFacts {
            ProcFacts::default()
        }
    }

    fn seq(sequence: u64) -> Birth {
        Birth {
            sequence: Some(sequence),
            time: None,
        }
    }

    fn time(time: u64) -> Birth {
        Birth {
            sequence: None,
            time: Some(time),
        }
    }

    fn both(sequence: u64, time: u64) -> Birth {
        Birth {
            sequence: Some(sequence),
            time: Some(time),
        }
    }

    fn tree(root: u32, procs: &[PROCESSENTRY32W], births: &[(u32, Birth)]) -> LocalProcessInfo {
        let info = procs
            .iter()
            .find(|info| info.th32ProcessID == root)
            .unwrap();
        let mut births = Births(births.iter().copied().collect());
        build_tree(
            info,
            procs,
            Birth::default(),
            0,
            &mut HashSet::new(),
            &mut births,
        )
        .unwrap()
    }

    fn pids(info: &LocalProcessInfo) -> Vec<u32> {
        fn walk(info: &LocalProcessInfo, out: &mut Vec<u32>) {
            out.push(info.pid);
            for child in info.children.values() {
                walk(child, out);
            }
        }
        let mut out = vec![];
        walk(info, &mut out);
        out.sort_unstable();
        out
    }

    fn depth(info: &LocalProcessInfo) -> usize {
        info.children
            .values()
            .map(|c| depth(c) + 1)
            .max()
            .unwrap_or(0)
    }

    /// The launcher (8) started the server (12) and exited; a program in
    /// the server's pane was then given pid 8, so the table makes the
    /// server a child of its own pane.
    fn server_below_its_own_pane() -> Vec<PROCESSENTRY32W> {
        vec![
            entry(12, 8),  // server, its launcher gone
            entry(16, 12), // pane shell
            entry(20, 16), // agent in the pane
            entry(8, 20),  // a program the agent ran, given the launcher's pid
        ]
    }

    #[test]
    fn a_reused_parent_pid_does_not_put_the_server_below_its_pane() {
        let births = [(12, seq(1)), (16, seq(2)), (20, seq(3)), (8, seq(4))];
        let pane = tree(16, &server_below_its_own_pane(), &births);
        assert_eq!(pids(&pane), vec![8, 16, 20]);
    }

    #[test]
    fn the_server_stays_out_when_the_reused_pid_has_exited() {
        // Pid 8 is gone by the time it is read: the server is measured
        // against the agent above it instead.
        let births = [(12, seq(1)), (16, seq(2)), (20, seq(3))];
        let pane = tree(16, &server_below_its_own_pane(), &births);
        assert_eq!(pids(&pane), vec![8, 16, 20]);
    }

    #[test]
    fn a_parent_loop_ends_when_no_birth_is_known() {
        // Nothing tells the server apart without births; the loop through
        // the pane's own shell still ends.
        let pane = tree(16, &server_below_its_own_pane(), &[]);
        assert_eq!(pids(&pane), vec![8, 12, 16, 20]);
    }

    #[test]
    fn a_clock_stepped_back_does_not_drop_a_child() {
        let procs = [entry(4, 0), entry(8, 4)];
        // The child is later in sequence but earlier on the clock.
        let births = [(4, both(10, 500)), (8, both(11, 400))];
        assert_eq!(pids(&tree(4, &procs, &births)), vec![4, 8]);
    }

    #[test]
    fn creation_times_decide_without_sequence_numbers() {
        let procs = [entry(4, 0), entry(8, 4), entry(12, 4)];
        let births = [(4, time(100)), (8, time(100)), (12, time(99))];
        assert_eq!(pids(&tree(4, &procs, &births)), vec![4, 8]);
    }

    #[test]
    fn the_idle_process_is_not_its_own_child() {
        let procs = [entry(0, 0), entry(4, 0)];
        assert_eq!(pids(&tree(0, &procs, &[])), vec![0, 4]);
    }

    #[test]
    fn a_runaway_chain_stops_at_the_depth_limit() {
        let procs: Vec<_> = (1..1000).map(|n| entry(4 * (n + 1), 4 * n)).collect();
        assert_eq!(depth(&tree(8, &procs, &[])), MAX_DEPTH);
    }
}
