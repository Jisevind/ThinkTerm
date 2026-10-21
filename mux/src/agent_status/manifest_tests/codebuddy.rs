//! Screen-rule tests for `manifests/codebuddy.toml`.
//!
//! The screens below are copied from live CodeBuddy 2.171.0 sessions rather
//! than invented: the dialogs and the finished-turn transcript are what the
//! CLI actually drew.
//!
//! They live outside [`engine`](crate::agent_status::engine) so adding an
//! agent does not grow the engine's generic test module; the manifest and
//! its tests are reviewed, and can be removed, together.

mod tests {
    use crate::agent_status::engine::{
        evaluate, manifest_id_for_alias, DetectionInput, ScreenVerdict,
    };
    use thinkterm_proto::AgentState;

    fn eval_codebuddy(screen: &str, title: &str) -> ScreenVerdict {
        evaluate(
            "codebuddy",
            &DetectionInput {
                screen,
                osc_title: title,
                osc_progress: "",
            },
        )
    }

    /// CodeBuddy installs as a `#!/usr/bin/env node` shim, so a pane's
    /// foreground leader is the runtime and the shim only appears in argv.
    #[test]
    fn codebuddy_identifies_through_its_node_shim() {
        let mut argv = || {
            Some(vec![
                "node".to_string(),
                "/home/u/.npm-global/bin/codebuddy".to_string(),
            ])
        };
        assert_eq!(
            crate::agent_status::proc_match::identify(
                "/usr/bin/node",
                &mut argv,
                &manifest_id_for_alias
            )
            .as_deref(),
            Some("codebuddy")
        );
        assert_eq!(manifest_id_for_alias("cbc").as_deref(), Some("codebuddy"));
        assert_eq!(
            manifest_id_for_alias("codebuddy-code").as_deref(),
            Some("codebuddy")
        );
    }

    /// While a turn runs CodeBuddy rewrites the OSC 0 title on every run
    /// state change, advancing these ten frames every 200 ms
    /// (`TerminalTitleUpdater.SPINNER_FRAMES`). The title is therefore a
    /// report, not a TUI redraw to be sampled.
    #[test]
    fn codebuddy_title_spinner_is_working() {
        for frame in [
            "\u{280B}", "\u{2819}", "\u{2839}", "\u{2838}", "\u{283C}", "\u{2834}", "\u{2826}",
            "\u{2827}", "\u{2807}", "\u{280F}",
        ] {
            let title = format!("{frame} add the CodeBuddy manifest");
            assert!(
                matches!(
                    eval_codebuddy("", &title),
                    ScreenVerdict::State(AgentState::Working)
                ),
                "frame {frame} should read as working"
            );
        }
    }

    /// The idle title carries U+2733, and the transcript above the prompt
    /// box keeps the finished turn's own status line and its "∴ Thought"
    /// heading. Neither may keep the pane reading as working once the turn
    /// is over, which is why no rule matches that line.
    #[test]
    fn codebuddy_finished_turn_is_idle_not_working() {
        let screen = "\
∴ Thought

● I'll start by cloning the repository.

✺ Add the CodeBuddy manifest… (719s · running REPL · ctrl+t to show todos · Task running for a while)
  └ Next: evaluate the integration
────────────────────────────────────────
>
────────────────────────────────────────
? for shortcuts  1 shell · press ↓ to view tasks  ← for agents   3.3M tokens
";
        // The marker the updater leaves behind is the rest state, and it
        // outranks the finished turn's own line left above the box.
        let idle = eval_codebuddy(screen, "\u{2733} add the CodeBuddy manifest");
        assert!(
            matches!(idle, ScreenVerdict::State(AgentState::Idle)),
            "the idle marker should read as idle, got {idle:?}"
        );
        // With no marker on the title at all, nothing here may read as
        // working; that case is left to the known-agent Idle fallback.
        let unmarked = eval_codebuddy(screen, "add the CodeBuddy manifest");
        assert!(
            matches!(unmarked, ScreenVerdict::NoMatch),
            "a finished turn without a marker is left to the fallback, got {unmarked:?}"
        );
    }

    /// Its reasoning models print "∴ Thinking..." while the thought runs
    /// and settle the same heading to "∴ Thought" afterwards.
    #[test]
    fn codebuddy_thinking_heading_is_working() {
        let screen = "● earlier output\n\n∴ Thinking...\n";
        assert!(matches!(
            eval_codebuddy(screen, ""),
            ScreenVerdict::State(AgentState::Working)
        ));
        assert!(matches!(
            eval_codebuddy("● earlier output\n\n∴ Thought\n", ""),
            ScreenVerdict::NoMatch
        ));
    }

    /// Every permission dialog numbers its choices and always offers
    /// "1. Yes"; only the confirm line differs from tool to tool.
    #[test]
    fn codebuddy_permission_dialog_is_blocked() {
        let screen = "\
● Read(src/main.rs)

╭────────────────────────────────────────╮
│ Bash command                           │
│                                        │
│   cargo test -p mux                    │
│   Run the mux tests                    │
│                                        │
│ Do you want to proceed?                │
│ ❯ 1. Yes                               │
│   2. Yes, and don't ask again for session
│   3. No, and tell CodeBuddy what to do differently (esc)
╰────────────────────────────────────────╯
";
        assert!(matches!(
            eval_codebuddy(screen, ""),
            ScreenVerdict::State(AgentState::Blocked)
        ));
        // The file tools ask their own question, with the same options.
        let screen = "\
│ Edit file                              │
│ Do you want to make this edit to main.rs? │
│ ❯ 1. Yes                               │
│   2. Yes, and don't ask again for session
";
        assert!(matches!(
            eval_codebuddy(screen, ""),
            ScreenVerdict::State(AgentState::Blocked)
        ));
        // The REPL tool asks "Are you sure you want to do this?" instead,
        // with a "Confirm" heading of its own. Copied from a live session.
        let screen = "\
∴ Thought

● REPL · Run echo hello-from-test
╭──────────────────────────────────────────────────────╮
│ Confirm                                              │
│                                                      │
│   REPL                                               │
│   code: \"return await Bash({ command: \"echo hello-from-test\" })\", description: \"Run echo hello-from-test\"
│                                                      │
│ Are you sure you want to do this?                    │
│                                                      │
│ > 1. Yes                                             │
│   2. Yes, and don't ask again this session (shift + tab)
│   3. No, and tell CodeBuddy what to do differently (escape)
│                                                      │
╰──────────────────────────────────────────────────────╯
";
        // The turn keeps its spinner on the title while the dialog waits,
        // so the blocker has to outrank the title's working rule.
        assert!(matches!(
            eval_codebuddy(screen, "\u{280B} Reply with hello"),
            ScreenVerdict::State(AgentState::Blocked)
        ));
    }

    /// AskUserQuestion always ends its option list with the free-text
    /// "Type something" row, above the panel's own key hints.
    #[test]
    fn codebuddy_question_panel_is_blocked() {
        let screen = "\
Which dataset should I use?

❯ 1. personal_photos
  2. screenshots
  3. Type something
     Submit
Enter to select · ↑/↓ to navigate · Esc to cancel
";
        assert!(matches!(
            eval_codebuddy(screen, ""),
            ScreenVerdict::State(AgentState::Blocked)
        ));
        // As with the permission dialog, the spinner stays on the title.
        assert!(matches!(
            eval_codebuddy(screen, "\u{2839} Reply with hello"),
            ScreenVerdict::State(AgentState::Blocked)
        ));
    }
}
