use std::sync::OnceLock;

static VERSION: OnceLock<&'static str> = OnceLock::new();
static TRIPLE: OnceLock<&'static str> = OnceLock::new();
static COMMIT_TIME: OnceLock<Option<i64>> = OnceLock::new();

pub fn assign_version_info(version: &'static str, triple: &'static str) {
    VERSION.set(version).unwrap();
    TRIPLE.set(triple).unwrap();
}

/// When the commit this build came from was made, seconds since the epoch;
/// set once at startup beside the version.
pub fn assign_commit_time(time: Option<i64>) {
    let _ = COMMIT_TIME.set(time);
}

pub fn wezterm_commit_time() -> Option<i64> {
    COMMIT_TIME.get().copied().flatten()
}

pub fn wezterm_version() -> &'static str {
    VERSION
        .get()
        .unwrap_or(&"someone forgot to call assign_version_info")
}

pub fn wezterm_target_triple() -> &'static str {
    TRIPLE
        .get()
        .unwrap_or(&"someone forgot to call assign_version_info")
}

pub fn running_under_wsl() -> bool {
    #[cfg(unix)]
    unsafe {
        let mut name: libc::utsname = std::mem::zeroed();
        if libc::uname(&mut name) == 0 {
            let version = std::ffi::CStr::from_ptr(name.version.as_ptr())
                .to_string_lossy()
                .into_owned();
            return version.contains("Microsoft");
        }
    };

    false
}
