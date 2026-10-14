//! The wake-up a settings page that reads something every few seconds
//! comes back by: what it shows changes elsewhere, and nothing else
//! repaints it when that happens.

use std::sync::Mutex;
use std::time::Duration;
use window::{Window, WindowOps};

/// One wake-up at a time for a page. Asking again while one is pending only
/// points it at the newest window, so a Settings window closed and opened
/// again before the wake-up comes is the one it repaints.
pub struct PollTimer {
    pending: Mutex<Option<Window>>,
}

impl PollTimer {
    pub const fn new() -> Self {
        Self {
            pending: Mutex::new(None),
        }
    }

    /// Repaints `window` once `due_in` has passed, when the page reads
    /// again. Asked from the page's paint alone, so leaving the page stops
    /// it after one last tick.
    pub fn arm(&'static self, window: Window, due_in: Duration) {
        if self.pending.lock().unwrap().replace(window).is_some() {
            return;
        }
        promise::spawn::spawn_into_main_thread(async move {
            smol::Timer::after(due_in).await;
            if let Some(window) = self.pending.lock().unwrap().take() {
                window.invalidate();
            }
        })
        .detach();
    }
}
