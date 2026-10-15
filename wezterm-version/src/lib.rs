pub fn wezterm_version() -> &'static str {
    // See build.rs
    env!("WEZTERM_CI_TAG")
}

/// When the commit this was built from was made, in seconds since the Unix
/// epoch. None for a build from outside a git checkout.
pub fn wezterm_commit_time() -> Option<i64> {
    // See build.rs
    env!("THINKTERM_COMMIT_TIME").parse().ok()
}

pub fn wezterm_target_triple() -> &'static str {
    // See build.rs
    env!("WEZTERM_TARGET_TRIPLE")
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_build_from_a_checkout_knows_when_its_commit_was_made() {
        // A source archive builds without git, and so without a commit time.
        let checkout = concat!(env!("CARGO_MANIFEST_DIR"), "/../.git");
        if !std::path::Path::new(checkout).exists() {
            return;
        }
        assert!(super::wezterm_commit_time().is_some_and(|time| time > 1_700_000_000));
    }
}
