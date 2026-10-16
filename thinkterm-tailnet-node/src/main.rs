//! `thinkterm-tailnet`: the machine's node in the tailnet, in a process of its
//! own. The mux server starts it while the settings turn the tailnet on (see
//! `thinkterm_tailnet::keep_node`), and it holds the node the way the server
//! once did itself (see `thinkterm_tailnet::follow`): by the node's lock, one
//! process at a time, answering the machine's other processes on the
//! tailnet's socket and writing the report they read.
//!
//! It ends with the process that started it, and once the settings turn the
//! tailnet off; the server starts it again when the tailnet is on.

#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::time::Duration;

/// How often it looks whether to end.
#[cfg(any(target_os = "macos", target_os = "linux"))]
const WATCH: Duration = Duration::from_secs(1);

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn main() {
    log_to_file();
    let parent = match parse_parent() {
        Ok(parent) => parent,
        Err(err) => {
            log::error!("{err}");
            eprintln!("thinkterm-tailnet: {err}");
            std::process::exit(2);
        }
    };
    log::info!("holding the tailnet's node for process {parent}");
    thinkterm_tailnet::follow(|| {});
    loop {
        // Already gone by the time this looks is gone all the same: the
        // process is handed to another parent as its own ends.
        if std::os::unix::process::parent_id() != parent {
            log::info!("process {parent}, which started this, has gone");
            break;
        }
        if thinkterm_tailnet::is_off() {
            log::info!("the tailnet is off");
            break;
        }
        // Ended for good: nothing acts on the settings any more. Started
        // anew by the mux server, it does again.
        if !thinkterm_tailnet::is_following() {
            log::error!("the thread that follows the settings has ended");
            std::process::exit(1);
        }
        std::thread::sleep(WATCH);
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn main() {
    eprintln!("thinkterm-tailnet: joining a tailnet is not supported on this platform");
    std::process::exit(2);
}

/// The process it ends with: `--parent <pid>`, which the mux server passes,
/// else whichever started it.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn parse_parent() -> Result<u32, String> {
    let mut args = std::env::args().skip(1);
    let mut parent = std::os::unix::process::parent_id();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--parent" => {
                parent = args
                    .next()
                    .and_then(|pid| pid.parse().ok())
                    .ok_or("--parent needs a process id")?;
            }
            _ => return Err(format!("unknown argument {arg:?}")),
        }
    }
    Ok(parent)
}

/// The log: a file of its own in the runtime folder, named as the mux
/// server's are, so that the app prunes it with theirs once it is a week
/// old. Readable by this user alone: a login link may be in it. Standard
/// error when the file cannot be had. Panics go to it too.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn log_to_file() {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Log(Mutex<Box<dyn Write + Send>>);

    impl log::Log for Log {
        fn enabled(&self, metadata: &log::Metadata) -> bool {
            metadata.level() <= log::Level::Info
        }

        fn log(&self, record: &log::Record) {
            if !self.enabled(record.metadata()) {
                return;
            }
            let since = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default();
            let line = format!(
                "{}.{:03} [{}] {}: {}\n",
                since.as_secs(),
                since.subsec_millis(),
                record.level(),
                record.target(),
                record.args()
            );
            if let Ok(mut out) = self.0.lock() {
                let _ = out.write_all(line.as_bytes());
            }
        }

        fn flush(&self) {}
    }

    let dir = &*config::RUNTIME_DIR;
    let path = dir.join(format!("thinkterm-tailnet-log-{}.txt", std::process::id()));
    let out: Box<dyn Write + Send> = match std::fs::create_dir_all(dir).and_then(|()| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&path)
    }) {
        Ok(file) => Box::new(file),
        Err(err) => {
            eprintln!("thinkterm-tailnet: logging to {}: {err}", path.display());
            Box::new(std::io::stderr())
        }
    };
    if log::set_boxed_logger(Box::new(Log(Mutex::new(out)))).is_ok() {
        log::set_max_level(log::LevelFilter::Info);
    }
    std::panic::set_hook(Box::new(|info| {
        log::error!(
            "panic: {info}\n{}",
            std::backtrace::Backtrace::force_capture()
        );
    }));
}
