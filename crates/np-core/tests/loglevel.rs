//! F33: the level vocabulary that decides which llama.cpp log lines reach stderr.
//!
//! Three things are pinned here, and all three are host-independent by design — this suite runs on every CI row, in every feature
//! shape, with no model file and no subprocess, because the alternative is a threshold rule whose mistakes only appear on a machine
//! that happens to load a model:
//!
//!   * [`LogLevel::forwards`] — the gate itself, over llama.cpp's own `ggml_log_level` numbers.
//!   * the five spellings users may write, and what an invented one costs them.
//!   * [`resolve_level`] — which source wins when the environment and the file disagree.
//!
//! What is *not* here: that installing a callback changes a real run's stderr. That needs the artifact and lives in
//! `np-cli/tests/cli.rs`, where it prints `NOT COVERAGE` rather than passing quietly without one.

use np_core::loglevel::{resolve_level, LogLevel};

/// llama.cpp's own numbering, copied from the pinned tree (`ggml/include/ggml.h:653-660`) rather than invented:
/// `NONE=0 DEBUG=1 INFO=2 WARN=3 ERROR=4 CONT=5`. If upstream renumbers, the table below fails and this file has to be
/// re-read against the new header — which is the intended cost of pinning a third party's numbers at all.
const NONE: i32 = 0;
const DEBUG: i32 = 1;
const INFO: i32 = 2;
const WARN: i32 = 3;
const ERROR: i32 = 4;
const CONT: i32 = 5;

#[test]
fn the_gate_forwards_at_or_above_its_threshold() {
    // One row per level at each threshold, so an off-by-one cannot hide in a corner nobody exercises.
    let table: &[(LogLevel, &[(i32, bool)])] = &[
        (
            LogLevel::Debug,
            &[
                (NONE, false),
                (DEBUG, true),
                (INFO, true),
                (WARN, true),
                (ERROR, true),
            ],
        ),
        (
            LogLevel::Info,
            &[
                (NONE, false),
                (DEBUG, false),
                (INFO, true),
                (WARN, true),
                (ERROR, true),
            ],
        ),
        (
            LogLevel::Warn,
            &[
                (NONE, false),
                (DEBUG, false),
                (INFO, false),
                (WARN, true),
                (ERROR, true),
            ],
        ),
        (
            LogLevel::Error,
            &[
                (NONE, false),
                (DEBUG, false),
                (INFO, false),
                (WARN, false),
                (ERROR, true),
            ],
        ),
        // `Off` is not llama.cpp's `NONE`: it is this project's choice to forward nothing, including an error.
        (
            LogLevel::Off,
            &[
                (NONE, false),
                (DEBUG, false),
                (INFO, false),
                (WARN, false),
                (ERROR, false),
            ],
        ),
    ];
    for &(threshold, rows) in table {
        for &(level, expected) in rows {
            assert_eq!(
                threshold.forwards(level),
                expected,
                "threshold {threshold:?} against raw level {level}"
            );
        }
    }
}

#[test]
fn a_continuation_line_rides_with_info() {
    // `GGML_LOG_LEVEL_CONT` extends the line already in progress — it is how load progress gets drawn, and in this build it is the *only*
    // kind of progress line: np-core takes `llama_model_default_params()`, so llama.cpp installs its own callback and that callback's whole
    // output is `.` at CONT (`src/llama.cpp:413-423`). Forwarding a fragment of a line whose opening was filtered out would print half a
    // sentence, so a continuation is treated as informational: visible when informational output is wanted, silent otherwise. This is a
    // decision (design §4), not a fact from the header — and the default being `warn` is what makes the dotted transcript disappear.
    assert!(
        LogLevel::Debug.forwards(CONT),
        "debug asks for more than info"
    );
    assert!(LogLevel::Info.forwards(CONT));
    assert!(
        !LogLevel::Warn.forwards(CONT),
        "the default must silence the dots"
    );
    assert!(!LogLevel::Error.forwards(CONT));
    assert!(!LogLevel::Off.forwards(CONT));
}

#[test]
fn an_unknown_level_number_is_not_forwarded() {
    // A level this file has never seen is a reason to write nothing, not a reason to guess. Writing something invented would put
    // bytes on the user's stderr that no header described.
    for threshold in [
        LogLevel::Debug,
        LogLevel::Info,
        LogLevel::Warn,
        LogLevel::Error,
    ] {
        assert!(
            !threshold.forwards(99),
            "{threshold:?} forwarded an unrecognised level"
        );
        assert!(!threshold.forwards(-1));
    }
}

#[test]
fn the_five_spellings_parse_and_nothing_else_does() {
    for (text, expected) in [
        ("off", LogLevel::Off),
        ("error", LogLevel::Error),
        ("warn", LogLevel::Warn),
        ("info", LogLevel::Info),
        ("debug", LogLevel::Debug),
    ] {
        assert_eq!(text.parse::<LogLevel>(), Ok(expected), "{text:?}");
        // Round-tripping matters because the config error path quotes the name it accepted.
        assert_eq!(expected.as_str(), text);
    }

    for bad in ["trace", "OFF", "", "warning", "3"] {
        let err = bad
            .parse::<LogLevel>()
            .expect_err("invented level accepted");
        let message = err.to_string();
        assert!(
            message.contains(&format!("{bad:?}")) || bad.is_empty(),
            "refusal of {bad:?} does not name the value: {message}"
        );
        for name in ["off", "error", "warn", "info", "debug"] {
            assert!(message.contains(name), "refusal omits `{name}`: {message}");
        }
    }
}

/// Precedence is this program's one config-versus-environment rule: **the file wins, the environment fills what the file left
/// unset**, and only when both are silent does the default apply. `config.rs` states it for `[models]` ("never an override of a file that
/// names one") and §12 rule 7 repeats it — a rule the human amended on 2026-11-04, so this key does not get to invent an exception. What
/// the hatch buys is therefore "name a level without editing a file", not "outvote a file that named one".
#[test]
fn the_file_wins_the_environment_fills_and_a_bad_value_refuses() {
    // File set: it wins even against a different env value, in both orders of specificity.
    assert_eq!(resolve_level(None, Some(LogLevel::Off)), Ok(LogLevel::Off));
    assert_eq!(
        resolve_level(Some("info"), Some(LogLevel::Off)),
        Ok(LogLevel::Off),
        "an environment value must not outvote a file that named one"
    );
    // File silent: the hatch decides.
    assert_eq!(resolve_level(Some("debug"), None), Ok(LogLevel::Debug));
    // Silent everywhere: warn, which keeps load failures audible and drops the transcript.
    assert_eq!(resolve_level(None, None), Ok(LogLevel::Warn));

    // An unrecognised env value refuses even when the file would have won: a typo in a hatch should be visible rather than
    // silently meaning "the file's value".
    let err = resolve_level(Some("deubg"), Some(LogLevel::Off)).expect_err("typo accepted");
    assert!(
        err.contains("NP_LLAMA_LOG_LEVEL"),
        "the refusal must name the variable the reader mistyped: {err}"
    );
    assert!(err.contains("deubg"), "and the value as written: {err}");
    for name in ["off", "error", "warn", "info", "debug"] {
        assert!(err.contains(name), "refusal omits `{name}`: {err}");
    }

    // An *empty* value counts as unset, which is this program's existing rule for environment text (`config.rs`'s `env_text`, where
    // `NP_MODELS_BASE_URL=` means "not configured"). Inventing "empty is a typo" for this one key would make two variables of the same
    // program disagree about what an empty value is.
    assert_eq!(resolve_level(Some(""), None), Ok(LogLevel::Warn));
    assert_eq!(
        resolve_level(Some(""), Some(LogLevel::Debug)),
        Ok(LogLevel::Debug)
    );
}
