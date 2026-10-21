//! Screen-rule tests for `manifests/omp.toml`.
//!
//! The titles below are the ones a live OMP 18.12.5 session emitted: the
//! idle `π >`, the working spinner frames, and the attention `π !` form.
//!
//! They live outside [`engine`](crate::agent_status::engine) so adding an
//! agent does not grow the engine's generic test module; the manifest and
//! its tests are reviewed, and can be removed, together.

mod tests {
    use crate::agent_status::engine::{
        evaluate, manifest_id_for_alias, DetectionInput, ScreenVerdict,
    };
    use thinkterm_proto::AgentState;

    fn eval_omp(title: &str) -> ScreenVerdict {
        evaluate(
            "omp",
            &DetectionInput {
                screen: "",
                osc_title: title,
                osc_progress: "",
            },
        )
    }

    /// OMP installs as a `#!/usr/bin/env bun` shim, so a pane's foreground
    /// leader is the runtime and the shim only appears in argv.
    #[test]
    fn omp_identifies_through_its_bun_shim() {
        let mut argv = || {
            Some(vec![
                "bun".to_string(),
                "/home/u/.bun/bin/omp".to_string(),
                "--no-session".to_string(),
            ])
        };
        assert_eq!(
            crate::agent_status::proc_match::identify(
                "/home/u/.bun/bin/bun",
                &mut argv,
                &manifest_id_for_alias
            )
            .as_deref(),
            Some("omp")
        );
        assert_eq!(manifest_id_for_alias("omp").as_deref(), Some("omp"));
        assert_eq!(manifest_id_for_alias("oh-my-pi").as_deref(), Some("omp"));
    }

    /// Every selectable `tui.titleSpinner` style has to read as working:
    /// Braille (the default), pulse, dots, and the ASCII line.
    #[test]
    fn omp_title_reports_working_for_every_spinner_style() {
        let frames = [
            "⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏", "○", "◔", "◑", "◕", "●", "-", "\\",
            "|", "/",
        ];
        for frame in frames {
            let title = format!("π {frame} omp-probe");
            assert!(
                matches!(eval_omp(&title), ScreenVerdict::State(AgentState::Working)),
                "{} should be working",
                title
            );
        }
        assert!(matches!(
            eval_omp("π ⠋"),
            ScreenVerdict::State(AgentState::Working)
        ));
        // Under WSL the spinner is a static ':'.
        assert!(matches!(
            eval_omp("π : omp-probe"),
            ScreenVerdict::State(AgentState::Working)
        ));
    }

    #[test]
    fn omp_title_reports_idle_and_attention() {
        for title in ["π >", "π > omp-probe"] {
            assert!(
                matches!(eval_omp(title), ScreenVerdict::State(AgentState::Idle)),
                "{} should be idle",
                title
            );
        }
        for title in ["π !", "π ! omp-probe"] {
            assert!(
                matches!(eval_omp(title), ScreenVerdict::State(AgentState::Blocked)),
                "{} should be blocked",
                title
            );
        }
    }

    /// `tui.titleState` off writes `π: <session>`; that says nothing about
    /// the run state, so the manifest must not guess.
    #[test]
    fn omp_title_without_state_is_not_guessed() {
        for title in ["π:", "π: omp-probe", "π", "π omp-probe", "", "omp"] {
            assert!(
                matches!(eval_omp(title), ScreenVerdict::NoMatch),
                "{} should not classify",
                title
            );
        }
    }
}
