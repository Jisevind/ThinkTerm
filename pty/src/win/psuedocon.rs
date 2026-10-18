use super::WinChild;
use crate::cmdbuilder::CommandBuilder;
use crate::win::procthreadattr::ProcThreadAttributeList;
use anyhow::{bail, ensure, Error};
use filedescriptor::{FileDescriptor, OwnedHandle};
use lazy_static::lazy_static;
use shared_library::shared_library;
use std::ffi::OsString;
use std::io::Error as IoError;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::io::{
    AsRawHandle, BorrowedHandle, FromRawHandle, IntoRawHandle, OwnedHandle as StdOwnedHandle,
};
use std::path::Path;
use std::sync::Mutex;
use std::{mem, ptr};
use winapi::shared::minwindef::DWORD;
use winapi::shared::winerror::{HRESULT, S_OK};
use winapi::um::fileapi::GetFileType;
use winapi::um::handleapi::*;
use winapi::um::libloaderapi::{GetModuleHandleW, GetProcAddress};
use winapi::um::processthreadsapi::*;
use winapi::um::winbase::FILE_TYPE_PIPE;
use winapi::um::winbase::{
    CREATE_UNICODE_ENVIRONMENT, EXTENDED_STARTUPINFO_PRESENT, STARTF_USESTDHANDLES, STARTUPINFOEXW,
};
use winapi::um::wincon::COORD;
use winapi::um::winnt::HANDLE;

pub type HPCON = HANDLE;

pub const PSUEDOCONSOLE_INHERIT_CURSOR: DWORD = 0x1;
pub const PSEUDOCONSOLE_RESIZE_QUIRK: DWORD = 0x2;
pub const PSEUDOCONSOLE_WIN32_INPUT_MODE: DWORD = 0x4;
#[allow(dead_code)]
pub const PSEUDOCONSOLE_PASSTHROUGH_MODE: DWORD = 0x8;

shared_library!(ConPtyFuncs,
    pub fn CreatePseudoConsole(
        size: COORD,
        hInput: HANDLE,
        hOutput: HANDLE,
        flags: DWORD,
        hpc: *mut HPCON
    ) -> HRESULT,
    pub fn ResizePseudoConsole(hpc: HPCON, size: COORD) -> HRESULT,
    pub fn ClosePseudoConsole(hpc: HPCON),
);

/// `ConptyPackPseudoConsole(hServerProcess, hRef, hSignal, phPC)`: a pseudo
/// console made of handles another process handed over.
type PackPseudoConsole = unsafe extern "system" fn(HANDLE, HANDLE, HANDLE, *mut HPCON) -> HRESULT;

struct ConPty {
    funcs: ConPtyFuncs,
    /// Only the conpty.dll shipped alongside has it; with the system's
    /// pseudo console nothing can be handed over.
    pack: Option<PackPseudoConsole>,
}

fn load_conpty() -> ConPty {
    // If the kernel doesn't export these functions then their system is
    // too old and we cannot run.
    let kernel = ConPtyFuncs::open(Path::new("kernel32.dll")).expect(
        "this system does not support conpty.  Windows 10 October 2018 or newer is required",
    );

    // We prefer to use a sideloaded conpty.dll and openconsole.exe host deployed
    // alongside the application.  We check for this after checking for kernel
    // support so that we don't try to proceed and do something crazy.
    if let Ok(sideloaded) = ConPtyFuncs::open(Path::new("conpty.dll")) {
        ConPty {
            funcs: sideloaded,
            pack: sideloaded_pack_function(),
        }
    } else {
        ConPty {
            funcs: kernel,
            pack: None,
        }
    }
}

fn sideloaded_pack_function() -> Option<PackPseudoConsole> {
    let name: Vec<u16> = "conpty.dll\0".encode_utf16().collect();
    let module = unsafe { GetModuleHandleW(name.as_ptr()) };
    if module.is_null() {
        return None;
    }
    let function =
        unsafe { GetProcAddress(module, b"ConptyPackPseudoConsole\0".as_ptr() as *const i8) };
    if function.is_null() {
        None
    } else {
        Some(unsafe { mem::transmute::<_, PackPseudoConsole>(function) })
    }
}

lazy_static! {
    static ref CONPTY: ConPty = load_conpty();
}

/// Whether the pseudo consoles of this process can be handed to another:
/// only with the conpty.dll shipped alongside.
pub fn handoff_supported() -> bool {
    CONPTY.pack.is_some()
}

pub struct PsuedoCon {
    con: HPCON,
}

unsafe impl Send for PsuedoCon {}
unsafe impl Sync for PsuedoCon {}

impl Drop for PsuedoCon {
    fn drop(&mut self) {
        unsafe { (CONPTY.funcs.ClosePseudoConsole)(self.con) };
    }
}

impl PsuedoCon {
    /// Duplicates of the three handles the pseudo console is made of -- its
    /// signal pipe, the reference that keeps it open and the console host
    /// process, in that order -- for a successor process to rebuild it with
    /// `from_handoff`. conpty.dll has no function that returns them: its
    /// HPCON points at exactly these three, so they are read from there, and
    /// only from the conpty.dll shipped alongside, which can pack them again.
    /// What is read is checked before it is used.
    pub fn handoff_handles(&self) -> Result<[StdOwnedHandle; 3], Error> {
        ensure!(
            CONPTY.pack.is_some(),
            "the system's pseudo console cannot be handed over"
        );
        let [signal, reference, server] = unsafe { ptr::read(self.con as *const [HANDLE; 3]) };
        let duplicate = |handle: HANDLE| {
            unsafe { BorrowedHandle::borrow_raw(handle as _) }.try_clone_to_owned()
        };
        let signal = duplicate(signal)?;
        let reference = duplicate(reference)?;
        let server = duplicate(server)?;
        ensure!(
            unsafe { GetFileType(signal.as_raw_handle() as _) } == FILE_TYPE_PIPE,
            "the pseudo console's signal handle is not a pipe"
        );
        let host = super::process_image_name(&server)?;
        ensure!(
            ["openconsole.exe", "conhost.exe"]
                .iter()
                .any(|name| host.eq_ignore_ascii_case(name)),
            "the pseudo console's host process is {host}, not a console host"
        );
        Ok([signal, reference, server])
    }

    /// The pseudo console a predecessor handed over (see `handoff_handles`).
    /// The handles become the console's own.
    pub fn from_handoff(handles: [StdOwnedHandle; 3]) -> Result<Self, Error> {
        let pack = CONPTY
            .pack
            .ok_or_else(|| anyhow::anyhow!("this conpty.dll cannot adopt a handed-over console"))?;
        let [signal, reference, server] = handles;
        let mut con: HPCON = INVALID_HANDLE_VALUE;
        let result = unsafe {
            pack(
                server.as_raw_handle() as _,
                reference.as_raw_handle() as _,
                signal.as_raw_handle() as _,
                &mut con,
            )
        };
        ensure!(
            result == S_OK,
            "failed to adopt a pseudo console: HRESULT {}",
            result
        );
        // Closed by ClosePseudoConsole from now on.
        let _ = (
            signal.into_raw_handle(),
            reference.into_raw_handle(),
            server.into_raw_handle(),
        );
        Ok(Self { con })
    }

    pub fn new(size: COORD, input: FileDescriptor, output: FileDescriptor) -> Result<Self, Error> {
        let mut con: HPCON = INVALID_HANDLE_VALUE;
        let result = unsafe {
            (CONPTY.funcs.CreatePseudoConsole)(
                size,
                input.as_raw_handle() as _,
                output.as_raw_handle() as _,
                PSUEDOCONSOLE_INHERIT_CURSOR
                    | PSEUDOCONSOLE_RESIZE_QUIRK
                    | PSEUDOCONSOLE_WIN32_INPUT_MODE,
                &mut con,
            )
        };
        ensure!(
            result == S_OK,
            "failed to create psuedo console: HRESULT {}",
            result
        );
        Ok(Self { con })
    }

    pub fn resize(&self, size: COORD) -> Result<(), Error> {
        let result = unsafe { (CONPTY.funcs.ResizePseudoConsole)(self.con, size) };
        ensure!(
            result == S_OK,
            "failed to resize console to {}x{}: HRESULT: {}",
            size.X,
            size.Y,
            result
        );
        Ok(())
    }

    pub fn spawn_command(&self, cmd: CommandBuilder) -> anyhow::Result<WinChild> {
        let mut si: STARTUPINFOEXW = unsafe { mem::zeroed() };
        si.StartupInfo.cb = mem::size_of::<STARTUPINFOEXW>() as u32;
        // Explicitly set the stdio handles as invalid handles otherwise
        // we can end up with a weird state where the spawned process can
        // inherit the explicitly redirected output handles from its parent.
        // For example, when daemonizing wezterm-mux-server, the stdio handles
        // are redirected to a log file and the spawned process would end up
        // writing its output there instead of to the pty we just created.
        si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        si.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
        si.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
        si.StartupInfo.hStdError = INVALID_HANDLE_VALUE;

        let mut attrs = ProcThreadAttributeList::with_capacity(1)?;
        attrs.set_pty(self.con)?;
        si.lpAttributeList = attrs.as_mut_ptr();

        let mut pi: PROCESS_INFORMATION = unsafe { mem::zeroed() };

        let (mut exe, mut cmdline) = cmd.cmdline()?;
        let cmd_os = OsString::from_wide(&cmdline);

        let cwd = cmd.current_directory();

        let res = unsafe {
            CreateProcessW(
                exe.as_mut_slice().as_mut_ptr(),
                cmdline.as_mut_slice().as_mut_ptr(),
                ptr::null_mut(),
                ptr::null_mut(),
                0,
                EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
                cmd.environment_block().as_mut_slice().as_mut_ptr() as *mut _,
                cwd.as_ref()
                    .map(|c| c.as_slice().as_ptr())
                    .unwrap_or(ptr::null()),
                &mut si.StartupInfo,
                &mut pi,
            )
        };
        if res == 0 {
            let err = IoError::last_os_error();
            let msg = format!(
                "CreateProcessW `{:?}` in cwd `{:?}` failed: {}",
                cmd_os,
                cwd.as_ref().map(|c| OsString::from_wide(c)),
                err
            );
            log::error!("{}", msg);
            bail!("{}", msg);
        }

        // Make sure we close out the thread handle so we don't leak it;
        // we do this simply by making it owned
        let _main_thread = unsafe { OwnedHandle::from_raw_handle(pi.hThread as _) };
        let proc = unsafe { OwnedHandle::from_raw_handle(pi.hProcess as _) };

        Ok(WinChild {
            proc: Mutex::new(proc),
        })
    }
}
