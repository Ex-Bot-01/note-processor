//! The `np models` commands end to end: real process, real exit codes, real artifact bytes.
//!
//! These cases drive the compiled binary rather than calling [`np_core::models`], because what the CLI owns is
//! exactly what a library test cannot reach: argv, the two output channels, the exit code, and the `--json` key
//! set §3 pins. The behaviour behind each outcome — which refusal comes first, what a digest is compared with —
//! is pinned in `crates/np-core/tests/models.rs`, and duplicated here only where the *channel* is the subject.
//!
//! A separate file from `cli.rs` on purpose: that suite's `Workspace::command` appends the workspace root last
//! and ~20 scan cases depend on that shape (§6's caution), while `hash` takes a file and no root at all. The
//! builder below is therefore its own, and it appends nothing implicitly.
//!
//! No SQL here either: after each run the state a command left behind is read back through `np models list`, so
//! this file needs neither `rusqlite` nor a second derivation of any path — every path it asserts on comes from
//! [`Config::workspace_models_dir`], which is what makes an assertion about artifact placement mean something.

//! Every `fetch` case dials a fixture this file owns — either [`Origin`], which answers 404 to everything it is
//! asked for, or [`DEAD_ORIGIN`], a port with nothing listening. None of them reaches the internet: since §12 rule
//! 7's amendment a fetch always resolves an origin, so a case that configured none would download 865 MB.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

// Only the `fetch` cases own a fake origin and read its socket log, so every import that exists to build or inspect one
// follows the feature. Without this the `--no-default-features` row — where all of those cases are compiled out — fails
// `-D warnings` on unused imports.
#[cfg(feature = "fetch")]
use std::io::{self, Read, Write};
#[cfg(feature = "fetch")]
use std::net::TcpListener;
#[cfg(feature = "fetch")]
use std::sync::atomic::AtomicBool;
#[cfg(feature = "fetch")]
use std::sync::atomic::Ordering;
#[cfg(feature = "fetch")]
use std::sync::{Arc, Mutex};
#[cfg(feature = "fetch")]
use std::thread;
#[cfg(feature = "fetch")]
use std::time::Duration;

use np_core::config::Config;
use np_core::hash::sha256_file;
use tempfile::TempDir;

/// The id one case installs. Lowercase and short because §2's charset rule is not cosmetic — it is a file name.
const ID: &str = "minilm";

/// A workspace directory that exists, with its own data dir.
struct Scratch {
    /// Kept alive so both directories outlive every run against them.
    _tmp: TempDir,
    root: PathBuf,
}

impl Scratch {
    fn new() -> Self {
        let tmp = TempDir::new().expect("test setup: tempdir");
        let root = tmp.path().join("ws");
        fs::create_dir_all(&root).expect("test setup: workspace dir");
        Scratch { _tmp: tmp, root }
    }

    /// `np` with every directory it can read a data dir or a config file from set to this tempdir — the same
    /// five variables `cli.rs` redirects, because on Windows the config dir comes from `%APPDATA%` and the data
    /// dir from `%LOCALAPPDATA%`, and a developer's own `config.toml` must not decide what these cases see.
    ///
    /// Nothing is appended: a models command spells its own operand. See this file's header for why that is not
    /// `cli.rs`'s helper.
    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_np"));
        cmd.env("XDG_DATA_HOME", self.data_dir())
            .env("LOCALAPPDATA", self.data_dir())
            .env("HOME", self._tmp.path())
            .env("XDG_CONFIG_HOME", self.config_dir())
            .env("APPDATA", self.config_dir());
        // Since §12 rule 7's amendment a fetch always has an origin, so these cases decide their own: a developer's
        // shell carrying `NP_MODELS_BASE_URL` must not point a suite at a real host, and one carrying
        // `NP_MODELS_SHA256` must not loosen a digest check. A case that wants either says so with `.env(…)`.
        cmd.env_remove("NP_MODELS_BASE_URL");
        cmd.env_remove("NP_MODELS_SHA256");
        // **F38**: the kill switch is removed for the same reason as the two above — a developer's shell must not decide
        // whether these cases fetched, and a case that wants it off sets it explicitly.
        cmd.env_remove("NP_NO_AUTO_FETCH");
        cmd.args(args);
        cmd
    }

    /// `np models <args> <workspace>` — the workspace operand last, so a case spells only its flags.
    fn models(&self, args: &[&str]) -> Command {
        let mut cmd = self.command(&["models"]);
        cmd.args(args).arg(&self.root);
        cmd
    }

    /// `np models install <workspace> --id <ID> --path <source> --dim <dim> --sha256 <sha256>`.
    ///
    /// The four flags are mandatory (§2), so every case that is not *about* argv passes them through here.
    fn install(&self, source: &Path, dim: &str, sha256: &str) -> Command {
        let mut cmd = self.models(&["install", "--id", ID]);
        cmd.arg("--path")
            .arg(source)
            .arg("--dim")
            .arg(dim)
            .arg("--sha256")
            .arg(sha256);
        cmd
    }

    /// `np models list`, optionally with `--json`.
    fn list(&self, json: bool) -> Command {
        let mut cmd = self.models(&["list"]);
        if json {
            cmd.arg("--json");
        }
        cmd
    }

    fn config_dir(&self) -> PathBuf {
        self._tmp.path().join("config")
    }

    fn data_dir(&self) -> PathBuf {
        self._tmp.path().join("data")
    }

    /// This workspace's artifact directory, **derived** the way the binary derives it rather than spelled out.
    ///
    /// A literal `<hash>.models` here would still pass against a build that derived the directory by another
    /// rule — which is §7's mutation "re-derive the artifact path in np-cli", the P0 arriving by another door.
    fn models_dir(&self) -> PathBuf {
        Config {
            data_dir: self.data_dir(),
            ..Config::default()
        }
        .workspace_models_dir(&self.root)
    }

    /// A source artifact the human supposedly already has, outside the workspace. Returns its path; its digest
    /// comes from [`source_digest`], never from a hash this file computes by hand.
    fn source(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self._tmp.path().join(name);
        fs::write(&path, bytes).expect("test setup: write the source artifact");
        path
    }
}

/// The digest to declare for `source`: [`sha256_file`], the same function `np models hash` calls. Coming from
/// the library rather than from a literal keeps a fixture honest when the file's bytes change — and this file
/// defining no second SHA-256 is the premise §7's test 9 pins.
fn source_digest(path: &Path) -> String {
    sha256_file(path).expect("test setup: digest the source artifact")
}

/// A finished run, with the command spelled out in every failure message (the `cli.rs` shape).
struct Run {
    output: Output,
    argv: String,
}

impl Run {
    fn stdout(&self) -> String {
        String::from_utf8_lossy(&self.output.stdout).into_owned()
    }

    /// Both channels as one string, for claims about **where the program reached**, which is a fact about the run rather
    /// than about a stream. Wording assertions still name their channel; only the F38 socket tests need both at once — and
    /// they exist only where a fetch can happen, so this helper is gated with them rather than warning as dead code on the
    /// `--no-default-features` row.
    #[cfg(all(feature = "local-model", feature = "fetch"))]
    fn combined(&self) -> String {
        format!("{}{}", self.stdout(), self.stderr())
    }

    fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.output.stderr).into_owned()
    }

    fn code(&self) -> i32 {
        self.output
            .status
            .code()
            .unwrap_or_else(|| panic!("`{}` was killed by a signal", self.argv))
    }

    fn assert_success(&self) {
        assert_eq!(
            self.code(),
            0,
            "`{}` failed: stdout `{}`, stderr `{}`",
            self.argv,
            self.stdout(),
            self.stderr()
        );
    }

    /// A refusal: non-zero, and the reason on stderr rather than in a panic.
    fn assert_refused(&self) {
        assert_ne!(
            self.code(),
            0,
            "`{}` refused but exited 0: stdout `{}`",
            self.argv,
            self.stdout()
        );
    }

    /// stdout parsed as the one JSON object `--json` promises.
    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.stdout()).unwrap_or_else(|err| {
            panic!(
                "`{}` printed no JSON object: {err}\n{}",
                self.argv,
                self.stdout()
            )
        })
    }
}

/// Run `cmd` and capture both descriptors.
fn run(cmd: &mut Command) -> Run {
    let argv = format!("{cmd:?}");
    Run {
        output: cmd
            .output()
            .unwrap_or_else(|err| panic!("cannot start `{argv}`: {err}")),
        argv,
    }
}

/// The keys of a JSON object, sorted, so an assertion can compare **the whole set**.
///
/// This is the shape §3's key-set pin has to be written in: looking one key up (`report["state"].is_some()`)
/// passes happily after `state` is renamed to `status`, which is exactly the drift §6 warns about. Comparing
/// the sorted set means a rename, an addition or a drop all fail.
fn keys(value: &serde_json::Value) -> Vec<String> {
    let object = value
        .as_object()
        .unwrap_or_else(|| panic!("expected a JSON object: {value}"));
    let mut names: Vec<String> = object.keys().cloned().collect();
    names.sort();
    names
}

/// The pinned top-level key set of `np models list --json` (§3), sorted for [`keys`].
///
/// **`fetching` is a deliberate protocol change**, not an accident of this round: `docs/design-m2-local-embeddings.md`
/// §12 rule 3 puts a fetch beside `unclaimed` rather than inside `ArtifactState`, because fetching is orthogonal to
/// all four verdicts (a first start has no row at all, and a present row can be re-fetching). Adding the key had to
/// fail this fence — three cases did, naming the new key on the left of every diff — and updating it here, with the
/// design section named as the authority, is the whole point of pinning a key set instead of printing one.
const LIST_KEYS: &[&str] = &[
    "fetching",
    "models",
    "models_dir_error",
    "pinned",
    "unclaimed",
];

/// The per-entry key set of the **`pinned`** section (**issue #3**): what a fetch of the workspace's configured model
/// would cost, in bytes this build pinned, named by the members it would write.
const PINNED_KEYS: &[&str] = &["bytes_total", "id", "members"];

/// The pinned per-row key set of `np models list --json` (§3), sorted for [`keys`].
const ROW_KEYS: &[&str] = &[
    "dim",
    "id",
    "installed_at",
    "members",
    "prefix_doc",
    "prefix_query",
    "sha256",
    "state",
];

/// The per-entry key set of a row's `members` array (**F28**'s gap closed): the member's name, the file it claims under
/// the models directory, and whether a file is there. No digest and no per-member state on purpose — `list` answers "did
/// the file land" by stat, and byte verification happens at use time, where reading 554 MB is already being paid for.
const MEMBER_KEYS: &[&str] = &["file", "name", "present"];

/// The pinned per-entry key set of `np models list --json`'s `fetching` array (§12 rules 4 and 12), sorted.
///
/// One shape for every marker state, the way `models_dir_error` works one level up: a marker that will not parse
/// answers with null fields plus `marker_error`, rather than a different set of keys — so a reader never has to
/// guess whether an absent key meant "no marker" or "unreadable marker".
const FETCH_KEYS: &[&str] = &[
    "bytes_done",
    "bytes_total",
    "id",
    "marker_error",
    "nonce",
    "path",
    "started_at",
];

/// Plant a fetch marker for `id` holding `body`, at the path the **library** derives for it.
///
/// Derived rather than spelled: a literal `.fetching.<id>.json` here would keep passing against a build whose
/// reader and writer disagreed about the name, which is the same reason [`Scratch::models_dir`] refuses to spell
/// the artifact directory.
fn plant_marker(ws: &Scratch, id: &str, body: &str) -> PathBuf {
    let path = np_core::models::marker_path(&ws.models_dir(), id);
    fs::create_dir_all(path.parent().expect("test setup: a marker has a parent"))
        .expect("test setup: the models dir");
    fs::write(&path, body).expect("test setup: plant the marker");
    path
}

/// The marker body these cases plant when the fields are not the subject.
const MARKER_BODY: &str = r#"{"nonce":"4242-1762000000123456789-0","started_at":1762000000,"bytes_done":43970560,"bytes_total":324839936}"#;

/// Install once, successfully, and return the source that was installed together with its digest.
fn install_one(ws: &Scratch) -> (PathBuf, String) {
    let source = ws.source("artifact.bin", b"the bytes one model stands for\n");
    let digest = source_digest(&source);
    run(&mut ws.install(&source, "384", &digest)).assert_success();
    (source, digest)
}

/// §7's portable fence for the P0: the artifact directory and the database must come from **one** key. Asserted
/// on the directory listing rather than through `Config::workspace_models_dir`, because deriving both sides with
/// the same call is exactly the circle that lets a re-derivation pass unseen — and unlike the non-UTF-8 workspace
/// case in `np-core/tests/models.rs` (unix-only), this one runs on Windows too.
#[test]
fn the_artifact_directory_and_the_database_share_one_stem() {
    let ws = Scratch::new();
    let source = ws.source("artifact.bin", b"the bytes one model stands for\n");
    let digest = source_digest(&source);
    run(&mut ws.install(&source, "384", &digest)).assert_success();

    let mut pairs: Vec<(String, String)> = fs::read_dir(ws.data_dir().join("workspaces"))
        .expect("test: the workspaces directory is readable")
        .map(|entry| entry.expect("test: readable entry").path())
        .filter_map(|path| {
            Some((
                path.file_stem()?.to_str()?.to_string(),
                path.extension()?.to_str()?.to_string(),
            ))
        })
        .collect();
    pairs.sort();
    let databases: Vec<&str> = pairs
        .iter()
        .filter(|(_, ext)| ext == "sqlite")
        .map(|(stem, _)| stem.as_str())
        .collect();
    let models_dirs: Vec<&str> = pairs
        .iter()
        .filter(|(_, ext)| ext == "models")
        .map(|(stem, _)| stem.as_str())
        .collect();
    let names: Vec<String> = pairs
        .iter()
        .map(|(stem, ext)| format!("{stem}.{ext}"))
        .collect();
    assert_eq!(
        databases.len(),
        1,
        "one workspace leaves exactly one database: {names:?}"
    );
    assert_eq!(
        models_dirs, databases,
        "and its artifact directory carries the very same stem — one key, so a row and its artifact cannot end up
         in different workspaces (§2's P0). Listing seen: {names:?}"
    );
}

/// §3's third outcome string end to end: delete the artifact and re-install must say `restored`, exit 0, and
/// carry the same word in `--json`. The three strings are pinned by the design, so renaming one has to break.
#[test]
fn reinstalling_after_the_artifact_vanished_says_restored_in_both_channels() {
    let ws = Scratch::new();
    let source = ws.source("artifact.bin", b"the bytes one model stands for\n");
    let digest = source_digest(&source);
    run(&mut ws.install(&source, "384", &digest)).assert_success();

    fs::remove_file(ws.models_dir().join(ID)).expect("test setup: delete the artifact");
    let restored = run(&mut ws.install(&source, "384", &digest));
    restored.assert_success();
    assert_eq!(
        restored.stdout(),
        "restored\n",
        "the text channel names the outcome"
    );

    let mut again = ws.install(&source, "384", &digest);
    again.arg("--json");
    let json = run(&mut again);
    json.assert_success();
    // Parsed rather than substring-hunted: the CLI prints pretty JSON, and an assertion that only matches one
    // spacing would be testing serde's formatter instead of §3's contract.
    let report = json.json();
    assert_eq!(
        keys(&report),
        ["id", "state"],
        "install's key set is exactly what §3 pins"
    );
    assert_eq!(
        report["state"], "already installed",
        "and the field carries the outcome"
    );
    assert_eq!(report["id"], ID);
}

#[test]
fn hash_refuses_a_directory_with_nothing_on_stdout() {
    let ws = Scratch::new();
    let mut cmd = ws.command(&["models", "hash"]);
    cmd.arg(&ws.root);
    let refused = run(&mut cmd);
    refused.assert_refused();
    assert_eq!(
        refused.stdout(),
        "",
        "a refusal prints no digest line to capture"
    );
}

#[test]
fn install_refuses_a_malformed_digest_through_the_cli() {
    let ws = Scratch::new();
    let source = ws.source("artifact.bin", b"the bytes one model stands for\n");
    let refused = run(&mut ws.install(&source, "384", "deadbeef"));
    refused.assert_refused();
    assert_eq!(
        refused.stdout(),
        "",
        "and no outcome word accompanies the refusal"
    );
}

#[test]
fn install_prints_the_outcome_word_and_places_the_artifact() {
    let ws = Scratch::new();
    let source = ws.source("artifact.bin", b"the bytes one model stands for\n");
    let digest = source_digest(&source);

    let installed = run(&mut ws.install(&source, "384", &digest));

    installed.assert_success();
    // One word — the outcome, not a sentence about it (§3's three strings). Pinned whole rather than as
    // "contains something", so a second line or a different spelling fails.
    assert_eq!(installed.stdout(), "installed\n");
    // The artifact is at the per-workspace path the *config* derives, with the source's bytes, and it is a copy:
    // the source is untouched (§3 step 6).
    let artifact = ws.models_dir().join(ID);
    assert_eq!(
        fs::read(&artifact).expect("the installed artifact is at the derived per-workspace path"),
        fs::read(&source).expect("the source survives an install")
    );
    // No temporary left behind: `<id>.<nonce>.tmp` is renamed, and a corpse would show up as unclaimed.
    assert_eq!(names_of(&ws.models_dir()), [ID.to_string()]);

    // And the same fact on the listing channel, in text mode: state, dim, the shortened digest (§3: "12 hex"),
    // never the full one — a column that printed all 64 characters would be `--json` with newlines.
    let listed = run(&mut ws.list(false));
    listed.assert_success();
    let text = listed.stdout();
    for expected in [ID, "present", "dim 384", &digest[..12]] {
        assert!(
            text.contains(expected),
            "`{expected}` missing from:\n{text}"
        );
    }
    assert!(
        !text.contains(&digest),
        "the text column shortens the digest, `--json` carries it whole:\n{text}"
    );
}

#[test]
fn install_json_names_the_state_and_a_repeat_says_already_installed() {
    let ws = Scratch::new();
    let (source, digest) = install_one(&ws);

    // The second call with the whole declared tuple is `already installed`, not a rewrite and not an error
    // (§3 step 5, no `--force` anywhere in §2). Asserted here because the *word* a re-install prints is part of
    // the pinned output contract.
    let mut again = ws.install(&source, "384", &digest);
    again.arg("--json");
    let again = run(&mut again);
    again.assert_success();
    let report = again.json();
    assert_eq!(report["state"], "already installed", "{report}");
    assert_eq!(report["id"], ID, "{report}");
    // One JSON shape for every outcome (§3's precedent at `cli.rs`'s `upgrade_orphans`): the same two keys on
    // this outcome as on `installed`, which the first install below re-checks from a clean workspace.
    assert_eq!(keys(&report), ["id", "state"]);

    let fresh = Scratch::new();
    let source = fresh.source("artifact.bin", b"the bytes one model stands for\n");
    let digest = source_digest(&source);
    let mut first = fresh.install(&source, "384", &digest);
    first.arg("--json");
    let first = run(&mut first);
    first.assert_success();
    let report = first.json();
    assert_eq!(keys(&report), ["id", "state"], "{report}");
    assert_eq!(report["state"], "installed", "{report}");
    assert_eq!(report["id"], ID, "{report}");

    // Nothing else in the listing changed either: one file, and `installed_at` is a real timestamp.
    let listed = run(&mut fresh.list(true)).json();
    assert_eq!(
        listed["models"].as_array().map(Vec::len),
        Some(1),
        "{listed}"
    );
    assert!(listed["models"][0]["installed_at"].as_i64().unwrap_or(0) > 0);
}

#[test]
fn a_digest_mismatch_refuses_naming_both_digests_and_prints_no_success() {
    let ws = Scratch::new();
    let source = ws.source("artifact.bin", b"the bytes that are really there\n");
    let found = source_digest(&source);
    // A well-formed digest that is simply not this file's — the truncated-download case §3 step 2 handles, and
    // the wrong-file case step 4 handles, are different refusals; this is the second.
    let declared = np_core::hash::sha256_bytes(b"a different artifact entirely\n");
    assert_ne!(
        declared, found,
        "test setup: the two digests have to differ"
    );

    let failed = run(&mut ws.install(&source, "384", &declared));

    failed.assert_refused();
    // Both digests, on stderr, in `ModelError`'s own sentence (§3 step 4: the file's digest is public
    // information about a file the human already holds).
    let stderr = failed.stderr();
    assert!(
        stderr.contains(&found),
        "the file's digest must be named:\n{stderr}"
    );
    assert!(
        stderr.contains(&declared),
        "the declared digest must be named:\n{stderr}"
    );
    assert!(
        stderr.contains(&source.to_string_lossy().into_owned()),
        "and the file they belong to:\n{stderr}"
    );
    // Nothing that reads as a success. stdout is asserted **empty**, which is stronger than looking for the word
    // "installed": `DigestMismatch`'s own message ends "nothing was installed", so a substring check on stderr
    // would be satisfied by the refusal itself. A command that printed its outcome word before calling
    // `models::install` puts a line here, and this assertion is what dies.
    assert_eq!(failed.stdout(), "");

    // The same refusal under `--json`: no object at all, because there is no outcome to report.
    let mut json = ws.install(&source, "384", &declared);
    json.arg("--json");
    let json = run(&mut json);
    json.assert_refused();
    assert_eq!(json.stdout(), "");

    // And the refusal is real: no artifact at the target, and no row naming it (§7 case 2). Read back through
    // `list`, so this file holds no SQL.
    assert!(
        !ws.models_dir().join(ID).exists(),
        "a refused install writes no artifact"
    );
    let listed = run(&mut ws.list(true));
    listed.assert_success();
    assert_eq!(
        listed.json()["models"].as_array().map(Vec::len),
        Some(0),
        "a refused install records no row: {}",
        listed.stdout()
    );
}

#[test]
fn list_on_an_empty_workspace_reports_both_counts_as_zero() {
    let ws = Scratch::new();

    let plain = run(&mut ws.list(false));

    plain.assert_success();
    // Two count lines and nothing under them: "looked and found none" has to read differently from "printed
    // nothing", which is the same reason `np scan` prints `skipped 0`.
    assert_eq!(plain.stdout(), "models     0\nunclaimed  0\n");

    let json = run(&mut ws.list(true));
    json.assert_success();
    let report = json.json();
    assert_eq!(keys(&report), LIST_KEYS, "{report}");
    assert_eq!(report["models"].as_array().map(Vec::len), Some(0));
    assert_eq!(report["unclaimed"].as_array().map(Vec::len), Some(0));
    // Present and null on the good path — suppressing the key would cost every reader a missing-key branch.
    assert!(
        report.get("models_dir_error").is_some() && report["models_dir_error"].is_null(),
        "one JSON shape for every state, not one per state: {report}"
    );
}

#[test]
fn list_json_holds_exactly_the_pinned_keys_and_names_an_unclaimed_leftover() {
    let ws = Scratch::new();
    let (_source, digest) = install_one(&ws);
    // Plant the exact spelling §3 step 6 promises a crash can leave: `<id>.<nonce>.tmp`, no row naming it.
    // Reachable by design (rename happens before the insert), so `list` reports it instead of claiming it.
    let leftover = format!("{ID}.999-1730000000000000000-0.tmp");
    fs::write(ws.models_dir().join(&leftover), b"half a copy")
        .expect("test setup: plant the leftover temp");

    let listed = run(&mut ws.list(true));

    // Unclaimed entries never change the exit code (§3).
    listed.assert_success();
    let report = listed.json();
    // The whole key set, at both levels. Renaming `state` to anything else, or `unclaimed`, or adding a key,
    // fails here — which is why §3 pins these names in the design document rather than in a format string.
    assert_eq!(keys(&report), LIST_KEYS, "{report}");
    let rows = report["models"]
        .as_array()
        .cloned()
        .unwrap_or_else(|| panic!("`models` is an array on every state: {report}"));
    assert_eq!(rows.len(), 1, "{report}");
    assert_eq!(keys(&rows[0]), ROW_KEYS, "{report}");
    assert_eq!(rows[0]["id"], ID);
    assert_eq!(rows[0]["dim"], 384);
    // The full digest here, where the text column shows twelve characters.
    assert_eq!(rows[0]["sha256"], digest.as_str());
    assert_eq!(rows[0]["prefix_doc"], "");
    assert_eq!(rows[0]["prefix_query"], "");
    assert_eq!(rows[0]["state"], "present");
    // The leftover, under its own heading and with its kind: a bare name could not say *directory*, which §3
    // asks for.
    let unclaimed = report["unclaimed"]
        .as_array()
        .cloned()
        .unwrap_or_else(|| panic!("`unclaimed` is an array on every state: {report}"));
    assert_eq!(keys(&unclaimed[0]), ["kind", "name"], "{report}");
    assert_eq!(unclaimed[0]["name"], leftover.as_str(), "{report}");
    assert_eq!(unclaimed[0]["kind"], "file", "{report}");
    // The claimed artifact is not in that list: `unclaimed` means no row names it, and one does.
    assert_eq!(unclaimed.len(), 1, "{report}");

    // Text mode says the same thing, under its own heading (§3), with the kind spelled out.
    let plain = run(&mut ws.list(false));
    plain.assert_success();
    let text = plain.stdout();
    assert!(
        text.contains(&format!("  {leftover}  file")),
        "the unclaimed section has to name the leftover and its kind:\n{text}"
    );
    assert!(text.starts_with("models     1\n"), "{text}");
}

#[test]
fn hash_prints_the_digest_the_library_computes() {
    // §7 case 9: one SHA-256, two callers. The command's output is compared with [`sha256_file`] over the same
    // bytes — a second implementation in np-cli (and with it a `sha2` dependency) could not survive this.
    let ws = Scratch::new();
    let file = ws.source("to-digest.bin", b"abc");

    let hashed = run(&mut ws.command(&["models", "hash", &file.to_string_lossy()]));

    hashed.assert_success();
    let text = hashed.stdout();
    let mut lines = text.lines();
    let line = lines.next().unwrap_or_default().to_string();
    assert!(
        lines.next().is_none(),
        "`hash` prints exactly one line:\n{text}"
    );
    let printed = line
        .split("  ")
        .next()
        .unwrap_or_else(|| panic!("`hash` prints `<sha256>  <path>`: {line}"));
    assert_eq!(
        printed,
        sha256_file(&file).expect("test setup: digest the file")
    );
    // Independent of the library: FIPS 180-4's worked example. Without it the comparison above would still pass
    // if both callers somehow shared a wrong digest.
    assert_eq!(
        printed, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        "sha256 of \"abc\" is a published constant: {line}"
    );
    // And the operand comes back as spelled, so the line pastes into a shell.
    assert_eq!(
        text,
        format!("{printed}  {}\n", file.display()),
        "`<sha256>  <path>`, two spaces, the spelling `sha256sum` accepts"
    );
}

#[test]
fn hash_reports_a_missing_file_and_exits_nonzero() {
    let ws = Scratch::new();
    let absent = ws._tmp.path().join("no-such-file.bin");

    let failed = run(&mut ws.command(&["models", "hash", &absent.to_string_lossy()]));

    failed.assert_refused();
    assert_eq!(failed.stdout(), "");
    // `HashError`'s own message, which already names the path.
    assert!(
        failed
            .stderr()
            .contains(&absent.to_string_lossy().into_owned()),
        "the refusal has to name the file: {}",
        failed.stderr()
    );
}

#[test]
fn an_unknown_or_missing_operand_is_claps_usage_error() {
    let ws = Scratch::new();

    // Required flags absent entirely: clap's usage error, not a runtime refusal.
    let missing = run(&mut ws.models(&["install"]));
    missing.assert_refused();
    assert_eq!(missing.stdout(), "", "clap reports on stderr");
    let text = missing.stderr();
    assert!(text.contains("Usage:"), "not a usage error:\n{text}");
    for flag in ["--id", "--path", "--dim", "--sha256"] {
        assert!(
            text.contains(flag),
            "the usage error has to name the missing `{flag}`:\n{text}"
        );
    }

    // An unknown flag.
    let unknown = run(&mut ws.models(&["list", "--nope"]));
    unknown.assert_refused();
    assert_eq!(unknown.stdout(), "");
    assert!(
        unknown.stderr().contains("--nope"),
        "the unknown flag has to be named: {}",
        unknown.stderr()
    );

    // A missing positional, and a command group with no subcommand.
    let no_file = run(&mut ws.command(&["models", "hash"]));
    no_file.assert_refused();
    assert!(no_file.stderr().contains("Usage:"), "{}", no_file.stderr());
    let no_command = run(&mut ws.command(&["models"]));
    no_command.assert_refused();
    assert!(
        no_command.stderr().contains("Usage:"),
        "{}",
        no_command.stderr()
    );

    // None of it touched the workspace: clap refuses before any command runs.
    assert!(
        !ws.models_dir().exists(),
        "a parse error writes no artifact directory"
    );
}

#[test]
fn a_declared_dim_outside_the_range_is_refused_before_anything_is_read() {
    let ws = Scratch::new();
    // A `--path` that does not exist: the dim is refused while parsing, so this never reaches a file access and
    // certainly never hashes anything (§2's ordering claim, asserted the way §7 case 3 asserts the id rule).
    let absent = ws._tmp.path().join("no-such-artifact.bin");
    let digest = np_core::hash::sha256_bytes(b"whatever was declared");

    let zero = run(&mut ws.install(&absent, "0", &digest));
    zero.assert_refused();
    assert_eq!(zero.stdout(), "");
    assert!(
        zero.stderr().contains("--dim cannot be 0"),
        "a dim-0 row would admit zero-byte vectors: {}",
        zero.stderr()
    );

    let over = run(&mut ws.install(&absent, "70000", &digest));
    over.assert_refused();
    assert!(
        over.stderr().contains("65535"),
        "the refusal has to name the ceiling: {}",
        over.stderr()
    );

    // The other side of both boundaries, or the two refusals above would pass under a parser that refused
    // everything: 1 and `u16::MAX` are inside the range and have to install.
    let source = ws.source("artifact.bin", b"the bytes one model stands for\n");
    let digest = source_digest(&source);
    for dim in ["1", "65535"] {
        let id = format!("{ID}-{dim}");
        let mut ok = ws.models(&["install", "--id", &id]);
        ok.arg("--path")
            .arg(&source)
            .arg("--dim")
            .arg(dim)
            .arg("--sha256")
            .arg(&digest);
        let ok = run(&mut ok);
        ok.assert_success();
    }
    assert_eq!(
        names_of(&ws.models_dir()),
        [format!("{ID}-1"), format!("{ID}-65535")],
        "both ends of the documented range install"
    );
}

#[test]
fn a_workspace_that_is_not_a_directory_is_refused_before_anything_is_written() {
    // §3 step 0, at the one seam where it can still be got wrong: `Config::workspace_db` derives a name for a
    // typo as happily as for a real path and `Db::open` creates directories, so the check has to run **before**
    // the database is opened. A build that opens first exits 0 here and leaves an empty database — and an
    // artifact directory — under a hash no scan will ever read.
    let ws = Scratch::new();
    let typo = ws._tmp.path().join("typo-in-the-name");
    let absent = ws._tmp.path().join("no-such-artifact.bin");
    let digest = np_core::hash::sha256_bytes(b"irrelevant: nothing is read");

    let install = run(&mut ws.command(&[
        "models",
        "install",
        "--id",
        ID,
        "--path",
        &absent.to_string_lossy(),
        "--dim",
        "384",
        "--sha256",
        &digest,
        &typo.to_string_lossy(),
    ]));

    install.assert_refused();
    assert_eq!(install.stdout(), "");
    assert!(
        install
            .stderr()
            .contains(&typo.to_string_lossy().into_owned()),
        "the refusal has to name the workspace operand it refused: {}",
        install.stderr()
    );

    let listed = run(&mut ws.command(&["models", "list", &typo.to_string_lossy()]));
    listed.assert_refused();
    assert_eq!(listed.stdout(), "");
    // Nothing was minted: not a database, not an artifact directory, not even the `workspaces` folder beside them.
    assert!(
        !ws.data_dir().join("workspaces").exists(),
        "a refused workspace writes no database and no artifact directory"
    );
}

#[test]
fn the_prefix_flags_reach_the_row_and_a_changed_prefix_is_refused() {
    // `--prefix-doc` and `--prefix-query` are the two flags nothing else in this file passes, so nothing else
    // would notice them being dropped: they are part of what an installed model *means*, which is why §3 step 5
    // compares them against an existing row rather than only the digest.
    let ws = Scratch::new();
    let source = ws.source("artifact.bin", b"the bytes one model stands for\n");
    let digest = source_digest(&source);
    let mut cmd = ws.install(&source, "384", &digest);
    cmd.arg("--prefix-doc")
        .arg("passage: ")
        .arg("--prefix-query")
        .arg("query: ");
    run(&mut cmd).assert_success();

    let listed = run(&mut ws.list(true));
    listed.assert_success();
    let report = listed.json();
    assert_eq!(report["models"][0]["prefix_doc"], "passage: ", "{report}");
    assert_eq!(report["models"][0]["prefix_query"], "query: ", "{report}");
    // Text mode says the same values, quoted so a trailing space is visible.
    let text = run(&mut ws.list(false)).stdout();
    assert!(
        text.contains("prefix_doc \"passage: \"") && text.contains("prefix_query \"query: \""),
        "{text}"
    );

    // Same id, same digest, different prefix: refused, and the row keeps what it had (§3 step 5's whole tuple).
    let mut changed = ws.install(&source, "384", &digest);
    changed.arg("--prefix-doc").arg("document: ");
    let refused = run(&mut changed);
    refused.assert_refused();
    assert_eq!(refused.stdout(), "", "a refusal prints no outcome word");
    assert!(
        refused.stderr().contains("prefix_doc"),
        "the refusal has to name the column that disagrees: {}",
        refused.stderr()
    );
    let after = run(&mut ws.list(true)).json();
    assert_eq!(after["models"][0]["prefix_doc"], "passage: ", "{after}");
    assert_eq!(after["models"][0]["state"], "present", "{after}");
}

#[test]
fn a_models_directory_that_cannot_be_listed_is_not_a_per_row_verdict() {
    // §7 case 12, on §7 test 7's portable fixture: the artifact directory is a **regular file**, so `read_dir`
    // fails on both OSes while the database stays writable and the row stays readable.
    let ws = Scratch::new();
    install_one(&ws);
    fs::remove_dir_all(ws.models_dir()).expect("test setup: remove the artifact directory");
    fs::write(ws.models_dir(), b"not a directory").expect("test setup: make it a regular file");

    let plain = run(&mut ws.list(false));

    plain.assert_refused();
    // The row is still there and its artifact really is gone — so a listing that degraded into per-row verdicts
    // would print `missing` against it, telling the human that every model lost its weights when in fact
    // nothing was looked at. That is the sentence §3 forbids, and this assertion is what dies if it appears.
    assert!(
        !plain.stdout().contains("missing"),
        "a directory nobody could read is not a verdict on any row:\n{}",
        plain.stdout()
    );
    assert_eq!(
        plain.stdout(),
        "",
        "no rows were inspected, so no rows are printed"
    );
    assert!(
        plain
            .stderr()
            .contains(&ws.models_dir().to_string_lossy().into_owned()),
        "the refusal names the directory it could not use: {}",
        plain.stderr()
    );

    // The machine-readable channel answers with the same state, in the same key set — `models_dir_error`
    // populated, no rows, non-zero exit.
    let json = run(&mut ws.list(true));
    json.assert_refused();
    let report = json.json();
    assert_eq!(keys(&report), LIST_KEYS, "{report}");
    assert_eq!(report["models"].as_array().map(Vec::len), Some(0));
    assert_eq!(report["unclaimed"].as_array().map(Vec::len), Some(0));
    // The new section follows the rule its two neighbours already keep on this path. An empty `fetching` here
    // means "nothing was looked at", not "nothing is downloading"; that distinction is carried by
    // `models_dir_error` and the non-zero exit, both of which therefore have to stay.
    assert_eq!(
        report["fetching"].as_array().map(Vec::len),
        Some(0),
        "a directory nobody looked at cannot be reported as idle: {report}"
    );
    let error = report["models_dir_error"]
        .as_str()
        .unwrap_or_else(|| panic!("the failure has to be reported under the pinned key: {report}"));
    assert!(
        error.contains(&ws.models_dir().to_string_lossy().into_owned()),
        "and it has to name the directory: {error}"
    );
}

/// The names one directory holds, sorted — the count and the spelling of what a command left behind, so a
/// temporary that was never renamed cannot hide inside "the artifact is there".
fn names_of(dir: &Path) -> Vec<String> {
    let mut found: Vec<String> = fs::read_dir(dir)
        .unwrap_or_else(|err| panic!("cannot read `{}`: {err}", dir.display()))
        .map(|entry| {
            entry
                .expect("test setup: readdir entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    found.sort();
    found
}

// ---------------------------------------------------------------------------
// R7a — `np models list`'s `fetching` section (`docs/design-m2-local-embeddings.md` §12 rules 2–4).
//
// **No command can create a marker yet**: the fetcher is R7b, behind a feature that does not exist. Every
// marker below is planted by hand, and what these cases pin is the human-visible half — the names, the reading,
// and the reporting. Nothing here implies that progress is displayed anywhere in production.
// ---------------------------------------------------------------------------

/// §12 rule 3's re-fetch half, through the CLI: an installed model that is fetching again keeps its row line *and*
/// gains a fetch line. Pinned at this level because the temptation to collapse the overlap lives in the printer.
#[test]
fn a_re_fetch_shows_the_installed_row_and_the_fetch_together() {
    let ws = Scratch::new();
    install_one(&ws);
    plant_marker(&ws, ID, MARKER_BODY);

    let listed = run(&mut ws.list(true));
    listed.assert_success();
    let report = listed.json();
    assert_eq!(report["models"][0]["id"], ID, "{report}");
    assert_eq!(report["models"][0]["state"], "present", "{report}");
    assert_eq!(
        report["fetching"].as_array().map(|v| v.len()),
        Some(1),
        "{report}"
    );
    assert_eq!(report["fetching"][0]["id"], ID, "{report}");
    assert!(
        report["unclaimed"].as_array().map(|v| v.is_empty()) == Some(true),
        "a marker for an installed id is not junk: {report}"
    );

    let plain = run(&mut ws.list(false));
    let text = plain.stdout();
    assert!(text.contains(&format!("{ID}  present")), "{text}");
    assert!(
        text.contains(&format!("1 fetch in progress · {ID} (")),
        "the row line and the fetch line both appear:\n{text}"
    );
}

/// A fetch in progress gets its own section in both channels, and neither reserved name is announced as junk.
#[test]
fn a_fetch_in_progress_is_listed_in_both_channels_and_is_never_unclaimed() {
    let ws = Scratch::new();
    install_one(&ws);
    // A second model, with no row yet — the first-start case §12 rule 3 exists for — plus the partial download
    // it is writing and one piece of genuine junk.
    plant_marker(&ws, "beta", MARKER_BODY);
    fs::write(
        ws.models_dir()
            .join(".part.beta.4242-1762000000123456789-0"),
        b"half of it",
    )
    .expect("test setup: plant the partial download");
    fs::write(
        ws.models_dir().join("notes-backup.txt"),
        b"not a model thing",
    )
    .expect("test setup: plant the junk");

    let listed = run(&mut ws.list(true));
    // A fetch never changes an exit code, any more than an unclaimed entry does.
    listed.assert_success();
    let report = listed.json();
    assert_eq!(keys(&report), LIST_KEYS, "{report}");

    let fetching = report["fetching"]
        .as_array()
        .cloned()
        .unwrap_or_else(|| panic!("`fetching` is an array on every state: {report}"));
    assert_eq!(fetching.len(), 1, "one marker, one entry: {report}");
    assert_eq!(keys(&fetching[0]), FETCH_KEYS, "{report}");
    assert_eq!(fetching[0]["id"], "beta");
    assert_eq!(fetching[0]["nonce"], "4242-1762000000123456789-0");
    assert_eq!(fetching[0]["started_at"], 1_762_000_000);
    assert_eq!(fetching[0]["bytes_done"], 43_970_560);
    assert_eq!(fetching[0]["bytes_total"], 324_839_936);
    assert!(
        fetching[0]["marker_error"].is_null(),
        "a marker that parses reports no error: {report}"
    );
    // The path is the library's own, so a human can act on §12 rule 4's advice without guessing where the
    // artifact directory is (its name is a hash of the workspace path).
    assert_eq!(
        fetching[0]["path"],
        np_core::models::marker_path(&ws.models_dir(), "beta")
            .to_string_lossy()
            .as_ref()
    );

    // The row list answers what it answered before this section existed: `fetching` is beside it, not a fifth
    // verdict inside it.
    assert_eq!(report["models"][0]["id"], ID);
    assert_eq!(report["models"][0]["state"], "present");

    // Neither `.fetching.beta.json` nor `.part.beta.…` is junk; the one file nothing recognises still is.
    let unclaimed: Vec<&str> = report["unclaimed"]
        .as_array()
        .expect("`unclaimed` is an array on every state")
        .iter()
        .map(|entry| entry["name"].as_str().expect("every entry has a name"))
        .collect();
    assert_eq!(unclaimed, ["notes-backup.txt"], "{report}");

    // Text mode: one line per fetch, between the rows and the unclaimed heading. The byte sizes come from
    // `np_core::catalog::size_human` — the program's only formatter — so this asserts the composition while the
    // literal below pins today's rendering of those two numbers.
    let plain = run(&mut ws.list(false));
    plain.assert_success();
    let text = plain.stdout();
    let line = format!(
        "1 fetch in progress · beta (started 1762000000, {} / {})",
        np_core::catalog::size_human(43_970_560),
        np_core::catalog::size_human(324_839_936)
    );
    assert!(
        text.contains(&line),
        "one line per fetch, naming the id, the start time and both sizes:\n{text}"
    );
    assert!(
        text.contains("41.9 MB / 309.8 MB"),
        "the sizes are the catalog's spelling, not a second formatter:\n{text}"
    );
    assert!(
        !text.contains(".part.beta."),
        "a partial download is a recognised name, so it must not appear as junk:\n{text}"
    );
    assert!(
        text.contains("  notes-backup.txt  file"),
        "the genuine leftover is still reported:\n{text}"
    );
    // The fetch line sits between the two sections it belongs beside.
    let at = |needle: &str| {
        text.find(needle)
            .unwrap_or_else(|| panic!("`{needle}` missing:\n{text}"))
    };
    assert!(at(&line) > at("models     1\n"), "{text}");
    assert!(at(&line) < at("unclaimed  1\n"), "{text}");
}

/// Two fetches: both listed, ordered by id, and the count says two.
#[test]
fn two_fetches_in_progress_pluralise_the_count_and_both_appear() {
    let ws = Scratch::new();
    plant_marker(&ws, "beta", MARKER_BODY);
    plant_marker(
        &ws,
        "alpha",
        r#"{"nonce":"1-0","started_at":1762000500,"bytes_done":1024,"bytes_total":4096}"#,
    );

    let listed = run(&mut ws.list(true));
    listed.assert_success();
    let report = listed.json();
    let ids: Vec<&str> = report["fetching"]
        .as_array()
        .expect("an array")
        .iter()
        .map(|entry| entry["id"].as_str().expect("every entry has an id"))
        .collect();
    assert_eq!(
        ids,
        ["alpha", "beta"],
        "ordered by id so output is deterministic"
    );

    let plain = run(&mut ws.list(false));
    plain.assert_success();
    let text = plain.stdout();
    // The count rides on every line, because each line has to be readable on its own when someone greps stdout
    // for `fetch`. Pinned plural and all, so the shape is a decision on the record.
    assert!(
        text.contains("2 fetches in progress · alpha (started 1762000500, "),
        "{text}"
    );
    assert!(
        text.contains("2 fetches in progress · beta (started 1762000000, "),
        "{text}"
    );
}

/// §12 rule 12: an unknown size is printed as unknown. A denominator this program invented would be a number a
/// human could divide by.
#[test]
fn a_fetch_with_an_unknown_size_says_so_and_invents_no_denominator() {
    let ws = Scratch::new();
    plant_marker(
        &ws,
        "beta",
        r#"{"nonce":"1-0","started_at":1762000000,"bytes_done":43970560}"#,
    );

    let listed = run(&mut ws.list(true));
    listed.assert_success();
    let report = listed.json();
    assert!(
        report["fetching"][0]["bytes_total"].is_null(),
        "an absent `bytes_total` is null, never 0: {report}"
    );

    let plain = run(&mut ws.list(false));
    plain.assert_success();
    let text = plain.stdout();
    assert!(
        text.contains("41.9 MB, size unknown"),
        "the words have to be there:\n{text}"
    );
    assert!(
        !text.contains('/'),
        "and no fraction with a fabricated bottom half:\n{text}"
    );
}

/// A truncated marker is reported in both channels, names the file, keeps its bytes on disk, and exits 0.
#[test]
fn a_truncated_marker_is_reported_and_left_where_it_is() {
    let ws = Scratch::new();
    let body = r#"{"nonce":"n","started_at":1762000000,"bytes_done"#;
    let path = plant_marker(&ws, "beta", body);

    let plain = run(&mut ws.list(false));
    // Reporting a marker this build cannot read is not a fault of the workspace: no non-zero exit.
    plain.assert_success();
    let text = plain.stdout();
    assert!(
        text.contains("1 fetch in progress · beta (unreadable marker "),
        "the line has to say the marker could not be read:\n{text}"
    );
    assert!(
        text.contains(&path.to_string_lossy().into_owned()),
        "and name the file a human has to look at:\n{text}"
    );
    assert!(
        text.contains("runs off the end of the file"),
        "and say what was wrong — this file stops mid-write, which is what its reason says:\n{text}"
    );

    let listed = run(&mut ws.list(true));
    listed.assert_success();
    let report = listed.json();
    assert_eq!(keys(&report["fetching"][0]), FETCH_KEYS, "{report}");
    assert_eq!(report["fetching"][0]["id"], "beta");
    // One shape for every state: the fields stay, as null, and `marker_error` says why.
    assert!(report["fetching"][0]["nonce"].is_null(), "{report}");
    assert!(report["fetching"][0]["started_at"].is_null(), "{report}");
    assert!(report["fetching"][0]["bytes_done"].is_null(), "{report}");
    let reason = report["fetching"][0]["marker_error"]
        .as_str()
        .unwrap_or_else(|| panic!("an unreadable marker names why: {report}"));
    // One value, two channels — the shape the scan receipts use: the text line is not a paraphrase of this
    // string, it is this string, so a report read yesterday can be compared with one opened today.
    assert!(
        plain.stdout().contains(reason),
        "the text channel carries the same sentence as `marker_error`: {reason} / {}",
        plain.stdout()
    );

    // Both runs reported it; neither removed it. §12 rule 4 makes liveness consent, so an implicit cleanup is
    // not this command's authority even for a file it cannot parse.
    assert_eq!(
        fs::read(&path).expect("test: the marker is still on disk"),
        body.as_bytes(),
        "`list` reports a truncated marker; deleting it is a human's call"
    );
}

/// Nothing fetching prints exactly what it printed before this section existed — no header, no zero count.
#[test]
fn nothing_fetching_prints_no_extra_line() {
    let ws = Scratch::new();
    install_one(&ws);
    fs::write(ws.models_dir().join("stray.tmp"), b"junk").expect("test setup: plant junk");

    let plain = run(&mut ws.list(false));
    plain.assert_success();
    let text = plain.stdout();
    // Four lines and no word about fetching: `models`, the row, `unclaimed`, the leftover. `list_on_an_empty_workspace_reports_both_counts_as_zero`
    // already pins the byte-exact output of an empty workspace, which is the same claim with fewer parts — this
    // case is here because it says the same thing about a workspace that *has* rows and leftovers, where a
    // "fetching 0" header would otherwise be tempting.
    assert_eq!(text.lines().count(), 4, "{text}");
    assert!(text.starts_with("models     1\n"), "{text}");
    assert!(text.contains("unclaimed  1\n"), "{text}");
    assert!(
        !text.contains("fetch"),
        "nothing is downloading, so nothing may say so — and nothing may announce a zero count either:\n{text}"
    );

    let listed = run(&mut ws.list(true));
    listed.assert_success();
    assert_eq!(
        listed.json()["fetching"].as_array().map(Vec::len),
        Some(0),
        "the key is present and empty, never suppressed"
    );
}

// --- `np models fetch` (§12) ---------------------------------------------------------------
//
// Every case below is `#[cfg(feature = "fetch")]`, and that costs something worth naming: a `--no-default-features` run
// compiles this section **out**, so its green proves only that nothing here leaks into a build without the command — not
// that fetch behaves correctly there, because it cannot. The default row is where these cases actually execute.
//
// These cases own argv, the two channels and the exit code. What the transfer itself does — streaming, hashing while
// counting, length-before-digest, publishing by rename — is pinned in `crates/np-core/tests/fetch.rs` (rules against a
// fake transport) and `fetch_http.rs` (the real client against a loopback origin), because driving 865 MB of pinned
// bytes through a child process would test the OS's page cache rather than this program.

/// Write a user-level `config.toml` where the binary looks, so a case can configure `[models]`.
///
/// The whole block is `fetch`-only: without that feature no case configures an origin, and an un-gated helper is dead code
/// that `-D warnings` rightly objects to on the `--no-default-features` row.
#[cfg(feature = "fetch")]
impl Scratch {
    fn write_user_config(&self, body: &str) {
        let dir = self.config_dir().join("note-processor");
        fs::create_dir_all(&dir).expect("test setup: config package dir");
        fs::write(dir.join("config.toml"), body).expect("test setup: write user config");
    }

    /// `np models fetch --id <id> [extra flags]`.
    fn fetch(&self, id: &str, extra: &[&str]) -> Command {
        let mut cmd = self.models(&["fetch", "--id", id]);
        cmd.args(extra);
        cmd
    }

    /// The same, with `[models] base_url` written first so the run dials `origin` and nothing else.
    ///
    /// Every fetch case in this file goes through here or through [`Scratch::fetch`] with an explicit reason not to:
    /// with a baked default origin there is no such thing as "nowhere to dial", so an unconfigured case would be a
    /// real outbound transfer of 865 MB inside `cargo test`.
    fn fetch_at(&self, id: &str, origin: &str, extra: &[&str]) -> Command {
        self.write_user_config(&format!("[models]\nbase_url = \"{origin}\"\n"));
        self.fetch(id, extra)
    }
}

/// A loopback port with nothing listening on it.
///
/// Verified on this host: `connect()` to `127.0.0.1:1` answers `ECONNREFUSED`, so a run that reached its transport
/// here fails at **connect** time — no request bytes are sent, and the refusal is a class (`transport failure`).
/// That asymmetry is what the capability cases below assert on.
#[cfg(feature = "fetch")]
const DEAD_ORIGIN: &str = "http://127.0.0.1:1";

/// The filename of one pinned member, taken from the pin table rather than typed here.
#[cfg(feature = "fetch")]
fn pinned_filename(id: &str, member: &str) -> String {
    np_core::pins::pinned_member(id, member)
        .unwrap_or_else(|| panic!("test setup: `{member}` should be pinned for `{id}`"))
        .filename
        .to_string()
}

/// A loopback origin that answers **every** request with `404` and records each request line.
///
/// It answers rather than staying silent on purpose: a socket nobody reads from cannot distinguish "this build
/// planned one member" from "this build planned two, and the second is waiting for a reply", so a wrong plan would
/// hang instead of failing. Here every plan is observable in [`Origin::requests`], which is what makes "the default
/// set is one file" an assertion rather than a hope.
///
/// `404` because no fixture serves 309 MB: the fetch has to refuse either way, and what this fixture owns is only
/// *which files were asked for*, never what was in them.
#[cfg(feature = "fetch")]
struct Origin {
    base_url: String,
    stop: Arc<AtomicBool>,
    requests: Arc<Mutex<Vec<String>>>,
    worker: Option<thread::JoinHandle<()>>,
}

#[cfg(feature = "fetch")]
impl Origin {
    fn serve() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test setup: bind a loopback port");
        let port = listener
            .local_addr()
            .expect("test setup: origin addr")
            .port();
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Vec::new()));
        listener
            .set_nonblocking(true)
            .expect("test setup: nonblocking listener");
        let thread_stop = Arc::clone(&stop);
        let thread_requests = Arc::clone(&requests);
        let worker = thread::spawn(move || {
            while !thread_stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let mut buffer = [0_u8; 2048];
                        let read = stream.read(&mut buffer).unwrap_or(0);
                        let first_line = String::from_utf8_lossy(&buffer[..read])
                            .lines()
                            .next()
                            .unwrap_or_default()
                            .to_string();
                        thread_requests
                            .lock()
                            .expect("test setup: request log")
                            .push(first_line);
                        let _ = stream
                            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
                    }
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        });
        Origin {
            base_url: format!("http://127.0.0.1:{port}"),
            stop,
            requests,
            worker: Some(worker),
        }
    }

    /// The request lines, in arrival order. Stops the server first, so the answer is complete rather than racy.
    fn requests(mut self) -> Vec<String> {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            worker.join().expect("test setup: origin thread");
        }
        // The worker is joined above, so this lock has exactly one holder and the answer cannot grow after it.
        self.requests
            .lock()
            .expect("test setup: request log")
            .clone()
    }
}

#[cfg(feature = "fetch")]
impl Drop for Origin {
    fn drop(&mut self) {
        // A case that never called [`Origin::requests`] still must not leak a thread holding the listener.
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(feature = "fetch")]
#[test]
fn the_default_member_set_of_this_build_is_the_primary_file_alone() {
    // §13's "fetch what this binary can use", asserted on the wire rather than in a table: with no `--member`, the
    // only file this run asks an origin for is the primary artifact. The projector is 554 MB of encoders a build
    // without image support cannot load, so asking for it would be waste dressed as convenience.
    //
    // Which builds this case can speak for: `crates/np-cli/Cargo.toml` declares no `vision` passthrough, so every
    // row that runs this binary is a text-only build and the two-member default set is not reachable from here. The
    // projector half of the rule is pinned where it can be executed —
    // `crates/np-core/tests/pins.rs::members_for_build_is_the_member_set_this_binary_can_use`.
    let origin = Origin::serve();
    let scratch = Scratch::new();
    let run = run(&mut scratch.fetch_at("embeddinggemma-2", &origin.base_url, &[]));

    // 404 is a refusal, and the fixture answers 404 to everything by design.
    assert_eq!(run.code(), 1, "{}", run.stderr());
    let asked = origin.requests();
    let primary = pinned_filename("embeddinggemma-2", "main");
    let projector = pinned_filename("embeddinggemma-2", "mmproj");
    assert_eq!(asked.len(), 1, "the default plan asked for {asked:?}");
    // The exact request line, not `contains(&primary)`: the projector's filename ends with the primary's
    // (`mmproj-embeddinggemma-2-Q8_0.gguf`), so a substring test passes on a run that asked for the wrong file.
    assert_eq!(
        asked[0].as_str(),
        format!("GET /{primary} HTTP/1.1"),
        "the primary file is what a text-only install needs: {asked:?}"
    );
    for line in &asked {
        assert!(
            !line.contains(&projector),
            "a build without image support requested the projector: {line}"
        );
    }
    // And the refusal it did print is a transport class, not a capability sentence about the member it never asked
    // for. §12 rule 8: no URL, either.
    let text = run.stderr();
    assert!(text.contains("not found"), "expected a class: {text}");
    assert!(!text.contains("image support"), "{text}");
    for forbidden in ["http://", "https://", "://", "127.0.0.1"] {
        assert!(!text.contains(forbidden), "{forbidden:?} in: {text}");
    }
}

#[cfg(feature = "fetch")]
#[test]
fn an_explicit_member_list_is_the_plan_and_the_default_set_never_widens_it() {
    // The default set is a default, not a mandate: `--member` spells the whole plan. The distinguishing request is
    // `audio`, which nothing pins — if argv were merged with (or replaced by) the build's default set, this run
    // would dial the origin for the primary file and refuse on `main`. It must instead refuse on `audio` **before**
    // byte one, so the recorded request count is the assertion that matters.
    let origin = Origin::serve();
    let scratch = Scratch::new();
    let run = run(&mut scratch.fetch_at(
        "embeddinggemma-2",
        &origin.base_url,
        &["--member", "main", "--member", "audio"],
    ));

    assert_eq!(run.code(), 1, "{}", run.stderr());
    let text = run.stderr();
    assert!(
        text.contains("audio"),
        "the unpinned member is named: {text}"
    );
    assert!(
        text.contains("no recorded digest"),
        "the reason is the pin table: {text}"
    );
    assert!(
        origin.requests().is_empty(),
        "a plan with an unpinned member requests nothing at all"
    );
}

#[cfg(all(feature = "fetch", not(feature = "vision")))]
#[test]
fn a_projector_request_on_a_build_without_image_support_names_the_capability() {
    // The one refusal §13 adds, and it is about a capability rather than a location: `mmproj` **is** pinned, so the
    // honest sentence is "this binary was built without image support", not "nothing pins that member".
    //
    // Gated off on a `vision` build (F28), where the sentence would be a lie — and its sibling below is what makes that
    // gate a claim rather than a way to keep the old assertion green by never running it.
    //
    // What this case proves about the network: `[models] base_url` names a port with nothing listening on it, so a
    // run that reached its transport would fail at connect with a class naming a member. The message that arrives
    // instead is the capability sentence — which pins refusal **before any connection is attempted**, a stronger
    // claim than "no bytes moved", and the one §12 rule 8's no-URL boundary depends on.
    let scratch = Scratch::new();
    let run = run(&mut scratch.fetch_at("embeddinggemma-2", DEAD_ORIGIN, &["--member", "mmproj"]));

    assert_eq!(run.code(), 1, "{}", run.stderr());
    let text = run.stderr();
    assert!(text.contains("mmproj"), "the member is named: {text}");
    assert!(
        text.contains("image support"),
        "the refusal must name the capability it is missing: {text}"
    );
    // Rule 8, and no accident of this host: a capability sentence carries no URL-shaped text at all.
    for forbidden in ["http", "://", "127.0.0.1"] {
        assert!(!text.contains(forbidden), "{forbidden:?} in: {text}");
    }
    assert!(
        !scratch.models_dir().exists(),
        "a capability refusal creates no state"
    );
}

#[cfg(all(feature = "fetch", feature = "vision"))]
#[test]
fn a_vision_build_asks_for_the_projector_instead_of_refusing_it() {
    // The other side of the capability gate, and the only CLI-level statement this round makes about a vision build
    // without 554 MB on disk: asking for `mmproj` is no longer a capability refusal. It reaches the transport — which is
    // why the origin is a port with nothing listening — and fails there.
    //
    // Two halves, both falsifiable. The **absence** of the capability sentence is the claim about F28; the **presence** of
    // `transport failure` is the proof that the run got *past* the capability gate rather than exiting early for some other
    // reason — without it, any new refusal before the transport would satisfy the first half. Revert np-cli's `vision`
    // feature and both halves fail at once, which is how F28's missing passthrough would reappear.
    let scratch = Scratch::new();
    let run = run(&mut scratch.fetch_at("embeddinggemma-2", DEAD_ORIGIN, &["--member", "mmproj"]));

    assert_eq!(
        run.code(),
        1,
        "a dead origin still refuses: {}",
        run.stderr()
    );
    let text = run.stderr();
    assert!(
        !text.contains("image support"),
        "a build with the image code must not claim to lack it: {text}"
    );
    assert!(
        text.contains("transport failure"),
        "the run has to reach its transport, which is what proves nothing refused it for want of a capability: {text}"
    );
}

#[cfg(feature = "fetch")]
#[test]
fn an_unconfigured_fetch_says_it_is_using_the_compiled_in_origin() {
    // §12 rule 7's amendment made "no origin configured" mean *dial the baked default* rather than *refuse*, and an empty
    // `NP_MODELS_BASE_URL` counts as unset by a rule this suite pins on purpose. So the silence is the hazard: a mirror whose
    // value was lost to a shell would spend 865 MB of egress with nothing on screen saying where the bytes came from. The run
    // refuses below before any byte moves, which is also what keeps this case off the network.
    let scratch = Scratch::new();
    // An empty file, which is what this fixture always meant: "a user config exists and says nothing here".
    // It used to write `[workspace]` — **an invented section**, accepted only because the root was lenient.
    // F35 refuses that shape (correctly: a section this build does not know is exactly the thing a user should
    // be told about), and this test caught it rather than CI or a user. Recorded in
    // `docs/design-f35-unknown-keys.md` §3 as the cost of the change, observed on our own fixture.
    scratch.write_user_config("");
    // An occupied target is refused by rule 5 ahead of the transfer, so the notice above it is observable without dialling.
    fs::create_dir_all(scratch.models_dir()).expect("test setup: models dir");
    fs::write(
        scratch.models_dir().join("embeddinggemma-2"),
        b"existing bytes",
    )
    .expect("test setup: occupied target");

    let mut cmd = scratch.fetch("embeddinggemma-2", &[]);
    let run = run(&mut cmd);

    assert_eq!(run.code(), 1, "{}\n{}", run.stdout(), run.stderr());
    let stdout = run.stdout();
    assert!(
        stdout.contains("compiled-in default"),
        "the run has to say which origin it resolved: {stdout}"
    );
    assert!(
        stdout.contains("[models] base_url"),
        "and where to change it: {stdout}"
    );
    // Rule 8 holds for the notice too: a class, never a host.
    for both in [run.stdout(), run.stderr()] {
        for forbidden in ["huggingface", "https://", "http://", "://"] {
            assert!(
                !both.contains(forbidden),
                "{forbidden:?} appeared in fetch output: {both}"
            );
        }
    }
}

#[cfg(feature = "fetch")]
#[test]
fn a_configured_origin_says_so_rather_than_staying_silent() {
    let scratch = Scratch::new();
    fs::create_dir_all(scratch.models_dir()).expect("test setup: models dir");
    fs::write(
        scratch.models_dir().join("embeddinggemma-2"),
        b"existing bytes",
    )
    .expect("test setup: occupied target");

    let mut cmd = scratch.fetch_at("embeddinggemma-2", DEAD_ORIGIN, &[]);
    let run = run(&mut cmd);

    assert_eq!(run.code(), 1, "{}\n{}", run.stdout(), run.stderr());
    let stdout = run.stdout();
    assert!(
        stdout.contains("from [models] base_url"),
        "a mirror is worth confirming by name-of-source: {stdout}"
    );
    assert!(
        !stdout.contains("compiled-in default"),
        "and the two sources are never both claimed: {stdout}"
    );
}

#[cfg(feature = "fetch")]
#[test]
fn fetch_refuses_a_member_this_build_does_not_pin_and_names_it() {
    // argv reaches the pin table, and a name that is not pinned is a refusal by name rather than a shrug. The origin
    // is configured here so the refusal that arrives is provably about the **member**, not about a missing URL.
    let scratch = Scratch::new();
    scratch.write_user_config("[models]\nbase_url = \"https://mirror.invalid/resolve/main\"\n");
    let mut cmd = scratch.fetch("embeddinggemma-2", &["--member", "audio"]);
    let run = run(&mut cmd);

    assert_eq!(run.code(), 1, "{}", run.stderr());
    let text = run.stderr();
    assert!(text.contains("audio"), "the member is named: {text}");
    assert!(
        text.contains("no recorded digest"),
        "the reason is that nothing pins it: {text}"
    );
    // §12 rule 8 in the CLI's own output: the configured origin does not appear.
    for forbidden in ["mirror.invalid", "https://", "://"] {
        assert!(
            !text.contains(forbidden),
            "{forbidden:?} leaked into a refusal: {text}"
        );
    }
    assert!(
        !scratch.models_dir().exists(),
        "a member refusal creates no state"
    );
}

#[cfg(feature = "fetch")]
#[test]
fn fetch_names_an_id_this_build_cannot_fetch() {
    // The id is argv's whole subject here, and an id with no pin is a different refusal from an id with no origin:
    // one is "this build has no expectation", the other is "nothing was configured".
    let scratch = Scratch::new();
    scratch.write_user_config("[models]\nbase_url = \"https://mirror.invalid/resolve/main\"\n");
    let mut cmd = scratch.fetch("some-model", &[]);
    let run = run(&mut cmd);

    assert_eq!(run.code(), 1, "{}", run.stderr());
    let text = run.stderr();
    assert!(text.contains("some-model"), "the id is named: {text}");
    assert!(
        text.contains("no artifact is pinned"),
        "the reason is the pin table, not the origin: {text}"
    );
    assert!(!text.contains("mirror.invalid"), "{text}");
}

#[cfg(feature = "fetch")]
#[test]
fn discard_marker_reports_the_absence_it_found_and_the_fetch_that_follows_refuses() {
    // Two facts, one run, on two channels: `--discard-marker` prints what it found (here: nothing to discard), and
    // the fetch that follows it still refuses. There is no longer a "no origin configured" refusal for the second
    // half — §12 rule 7's amendment baked a default — so the origin here is [`DEAD_ORIGIN`] and the refusal that
    // arrives is a transport class.
    //
    // Which network claim this pins: nothing listens on that port, so the connection is refused at connect time.
    // That proves **no connection was established**, which is more than "no bytes moved": the client never got far
    // enough to send a request.
    let scratch = Scratch::new();
    let run = run(&mut scratch.fetch_at("embeddinggemma-2", DEAD_ORIGIN, &["--discard-marker"]));

    assert_eq!(run.code(), 1, "{}", run.stderr());
    assert!(
        run.stdout().contains("nothing to discard"),
        "the discard line is printed on stdout: {:?}",
        run.stdout()
    );
    let text = run.stderr();
    assert!(
        text.contains("transport failure"),
        "the fetch that followed the discard refused on the transport: {text}"
    );
    for forbidden in ["http://", "https://", "://", "127.0.0.1"] {
        assert!(!text.contains(forbidden), "{forbidden:?} in: {text}");
    }
}

#[cfg(feature = "fetch")]
#[test]
fn fetch_is_a_subcommand_of_models_and_not_of_scan() {
    // §12 rule 1's boundary, asserted where argv lives: `fetch` exists under `models`, and `np scan` grows no flag that
    // could reach it. A future "auto-fetch on scan" flag would have to be added here, and this case is where its
    // absence is noticed.
    let scratch = Scratch::new();
    let mut help_cmd = scratch.command(&["models", "--help"]);
    let help = run(&mut help_cmd);
    assert_eq!(help.code(), 0, "{}", help.stderr());
    assert!(help.stdout().contains("fetch"), "{}", help.stdout());

    let mut scan_cmd = scratch.command(&["scan", "--help"]);
    let scan_help = run(&mut scan_cmd);
    assert_eq!(scan_help.code(), 0, "{}", scan_help.stderr());
    for forbidden in ["fetch", "member", "base_url"] {
        assert!(
            !scan_help.stdout().contains(forbidden),
            "`np scan` must offer no path to a fetch: {forbidden:?} in its help"
        );
    }
}

#[cfg(feature = "fetch")]
#[test]
fn discard_marker_removes_the_marker_and_the_partial_it_names() {
    // The other branch of the same flag, and the one that matters: a marker left by a dead process, plus the
    // `.part.<id>.<nonce>` **that marker names**. Both go; a partial download belonging to some other nonce stays,
    // because nothing in this run can tell whether it belongs to a fetch that is still going.
    let scratch = Scratch::new();
    let models_dir = scratch.models_dir();
    fs::create_dir_all(&models_dir).expect("test setup: models dir");
    fs::write(
        models_dir.join(".fetching.embeddinggemma-2.json"),
        r#"{"nonce":"4242-1-0","started_at":1730000000,"bytes_done":999,"bytes_total":null}"#,
    )
    .expect("test setup: plant a marker from a dead fetch");
    let named_part = models_dir.join(".part.embeddinggemma-2.4242-1-0");
    fs::write(&named_part, b"half a model").expect("test setup: plant the partial it names");
    let other_part = models_dir.join(".part.embeddinggemma-2.7777-9-9");
    fs::write(&other_part, b"someone else's bytes")
        .expect("test setup: plant an unclaimed partial");

    // The discard is a filesystem action and runs first, so it happens even against an origin that cannot answer:
    // [`DEAD_ORIGIN`] makes the fetch that follows refuse at connect rather than reach a real host.
    let run = run(&mut scratch.fetch_at("embeddinggemma-2", DEAD_ORIGIN, &["--discard-marker"]));

    assert_eq!(run.code(), 1, "{}", run.stderr());
    assert!(
        run.stdout().contains("discarded") && run.stdout().contains("1 partial file(s) removed"),
        "the report says what went: {:?}",
        run.stdout()
    );
    assert!(
        !models::marker_path(&models_dir, "embeddinggemma-2").exists(),
        "the marker is gone"
    );
    assert!(!named_part.exists(), "the partial the marker named is gone");
    assert!(
        other_part.exists(),
        "a partial no marker claims is left for `list` to report, not deleted on a guess"
    );
}

/// The marker path comes from the library rather than a literal, so a rename in `np_core::models` moves this case
/// instead of leaving it asserting against a file nothing reads.
#[cfg(feature = "fetch")]
mod models {
    use std::path::{Path, PathBuf};

    pub fn marker_path(models_dir: &Path, id: &str) -> PathBuf {
        np_core::models::marker_path(models_dir, id)
    }
}

// --- F28 step 3: `--member`, so a machine with no network can place the projector ---
//
// `projector-absent` and the vision build both tell an operator to install the `mmproj` member. Until this flag existed
// the only way to place that file was `np models fetch`, which dials out — so on the machines where a 554 MB download is
// the reason someone installed a file by hand, the advice could not be followed. These cases run with no transport at all:
// the `fetch` feature is off in the default test build, which makes "offline" a property of the binary under test rather
// than of a mock.

/// `np models install <workspace> --id … --member <member> --path … --sha256 …`, with **no `--dim`**: a member install
/// declares no width, which is what separates it from [`Scratch::install`].
fn member_install(ws: &Scratch, member: &str, source: &Path, sha256: &str) -> Command {
    let mut cmd = ws.models(&["install", "--id", ID]);
    cmd.arg("--member")
        .arg(member)
        .arg("--path")
        .arg(source)
        .arg("--sha256")
        .arg(sha256);
    cmd
}

/// **A projector cannot be the first thing installed.** The member row describes a file belonging to an installed
/// identity, and this refusal is where an operator learns the order rather than discovering it through a database error.
#[test]
fn a_projector_install_without_a_model_names_the_missing_identity() {
    let ws = Scratch::new();
    let mmproj = ws.source("mmproj.gguf", b"projector-shaped bytes");
    let digest = source_digest(&mmproj);

    let refused = run(&mut member_install(&ws, "mmproj", &mmproj, &digest));
    refused.assert_refused();
    assert_eq!(refused.stdout(), "", "a refusal prints nothing on stdout");
    let text = refused.stderr();
    assert!(text.contains(ID), "the id is named: {text}");
    assert!(
        text.contains("nothing to belong to"),
        "and so is the reason: {text}"
    );
    assert!(
        !ws.models_dir().join(format!("{ID}.mmproj")).exists(),
        "a refusal leaves no artifact behind"
    );
    // Mutation this catches: recording the member before checking for the row, which is a 554 MB file describing an
    // identity nothing installed.
}

/// **The install lands, and the model row keeps saying what the text encoder is.** `dim` and `sha256` in the listing are
/// the whole point of this assertion: a projector digest written into `models.sha256` would make every later verification
/// compare the wrong bytes, and it would still leave both files readable. The empty `unclaimed` half is the other fact —
/// `<id>.mmproj` is claimed by the member table, not junk in the artifact directory.
#[test]
fn a_projector_install_reports_installed_and_leaves_the_model_row_alone() {
    let ws = Scratch::new();
    // The model first, from `install_one`, so the width and digest asserted below are ones this test did not invent.
    let (_main_source, main_digest) = install_one(&ws);
    let mmproj = ws.source(
        "mmproj.gguf",
        b"projector-shaped bytes, not the model's own",
    );
    let digest = source_digest(&mmproj);

    let installed = run(&mut member_install(&ws, "mmproj", &mmproj, &digest));
    installed.assert_success();
    assert_eq!(installed.stdout(), "installed\n", "one word, as §3 pins");
    assert_eq!(
        fs::read(ws.models_dir().join(format!("{ID}.mmproj")))
            .expect("the projector is where the member name says"),
        fs::read(&mmproj).expect("re-read the source"),
        "copied, not moved: the source stays where the human put it"
    );

    let report = run(&mut ws.list(true)).json();
    let row = &report["models"][0];
    assert_eq!(
        row["dim"], 384,
        "the width still describes the text encoder"
    );
    assert_eq!(
        row["sha256"],
        main_digest.as_str(),
        "and so does its digest"
    );
    assert_eq!(
        report["unclaimed"].as_array().map(Vec::len),
        Some(0),
        "the projector is a member of this model, not an unclaimed leftover: {:?}",
        report["unclaimed"]
    );

    // A second identical call repeats the §3 wording rather than rewriting anything.
    let again = run(&mut member_install(&ws, "mmproj", &mmproj, &digest));
    again.assert_success();
    assert_eq!(again.stdout(), "already installed\n");

    // Mutation this catches: routing a member install through the main path, which would demand a width and then compare
    // the whole tuple against `models.sha256` — visible above as either a refusal or a changed digest.
}

/// **A projector has no width, so `--dim` alongside it is a mistake worth naming.** Silently ignoring a flag that reads
/// like a fact about the file would be the quieter failure; this refuses and says which member the flag belongs to.
#[test]
fn a_projector_install_refuses_a_dim_because_a_projector_has_no_width() {
    let ws = Scratch::new();
    install_one(&ws);
    let mmproj = ws.source("mmproj.gguf", b"projector-shaped bytes");
    let digest = source_digest(&mmproj);

    let mut cmd = member_install(&ws, "mmproj", &mmproj, &digest);
    cmd.arg("--dim").arg("768");
    let refused = run(&mut cmd);
    refused.assert_refused();
    let text = refused.stderr();
    assert!(text.contains("--dim"), "the flag is named: {text}");
    assert!(
        text.contains("mmproj"),
        "and so is the member that has no width: {text}"
    );
    assert_eq!(
        fs::read_dir(ws.models_dir()).expect("listable").count(),
        1,
        "the model's own artifact only"
    );
}

/// **A member name nothing installs is refused, and says what is installable.** The set lives in np-core
/// (`models::INSTALLABLE_MEMBERS`) precisely so this message and the write path cannot drift into disagreeing.
#[test]
fn an_unknown_member_is_refused_with_the_names_that_are_installable() {
    let ws = Scratch::new();
    install_one(&ws);
    let mmproj = ws.source("mmproj.gguf", b"projector-shaped bytes");
    let digest = source_digest(&mmproj);

    let refused = run(&mut member_install(&ws, "audio", &mmproj, &digest));
    refused.assert_refused();
    let text = refused.stderr();
    assert!(
        text.contains("audio"),
        "the invented name is quoted: {text}"
    );
    for legal in np_core::models::INSTALLABLE_MEMBERS {
        assert!(text.contains(legal), "`{legal}` should be offered: {text}");
    }
    assert_eq!(
        fs::read_dir(ws.models_dir()).expect("listable").count(),
        1,
        "a refused member install writes no file"
    );
}

/// **`--member main` is the ordinary install**, not a second code path with different rules: same flags, same word.
#[test]
fn naming_the_main_member_installs_the_model_exactly_as_before() {
    let ws = Scratch::new();
    let source = ws.source("artifact.bin", b"the bytes one model stands for\n");
    let digest = source_digest(&source);
    let mut cmd = member_install(&ws, "main", &source, &digest);
    cmd.arg("--dim").arg("384");
    run(&mut cmd).assert_success();
    assert_eq!(run(&mut ws.list(true)).json()["models"][0]["dim"], 384);
}

// --- the listing says what a model's file set is ---------------------------------

/// **A projector installed offline shows up on the next `np models list`,** in both channels. Before this, the only way to
/// confirm that install was to run `np embed` and watch `projector-absent` go away — a diagnosis standing in for a report.
#[test]
fn an_installed_projector_is_listed_as_a_member_of_its_model() {
    let ws = Scratch::new();
    install_one(&ws);
    let mmproj = ws.source("mmproj.gguf", b"projector-shaped bytes");
    let digest = source_digest(&mmproj);
    run(&mut member_install(&ws, "mmproj", &mmproj, &digest)).assert_success();

    let report = run(&mut ws.list(true)).json();
    let members = report["models"][0]["members"]
        .as_array()
        .unwrap_or_else(|| panic!("`members` must serialise as an array: {report}"));
    assert_eq!(members.len(), 1, "{report}");
    assert_eq!(keys(&members[0]), MEMBER_KEYS, "{report}");
    assert_eq!(members[0]["name"], "mmproj", "{report}");
    assert_eq!(members[0]["file"], format!("{ID}.mmproj"), "{report}");
    assert_eq!(members[0]["present"], true, "{report}");

    let text = run(&mut ws.list(false)).stdout();
    assert!(
        text.contains("files mmproj:present"),
        "the text channel names the member too:\n{text}"
    );

    // Delete the file by hand and both channels change their answer, without any byte being hashed to decide it.
    fs::remove_file(ws.models_dir().join(format!("{ID}.mmproj")))
        .expect("test setup: delete the member");
    let after = run(&mut ws.list(true)).json();
    assert_eq!(
        after["models"][0]["members"][0]["present"], false,
        "{after}"
    );
    let text = run(&mut ws.list(false)).stdout();
    assert!(
        text.contains("files mmproj:absent"),
        "a recorded member with no file has to be sayable:\n{text}"
    );

    // Mutation this catches: dropping the array from `--json` while keeping the stat, which leaves the machine reader
    // exactly where F28 left it.
}

/// **A model with one file answers `[]`, and its text line says nothing about files.** The empty case is the common one,
/// and it has to be a fact rather than an omission — while the main artifact stays out of the list, because this row's own
/// `state` already gives a stronger verdict on it.
#[test]
fn a_model_with_only_its_main_artifact_lists_no_members() {
    let ws = Scratch::new();
    install_one(&ws);

    let report = run(&mut ws.list(true)).json();
    assert_eq!(
        report["models"][0]["members"].as_array().map(Vec::len),
        Some(0),
        "present and empty, never absent: {report}"
    );
    let text = run(&mut ws.list(false)).stdout();
    assert!(
        !text.contains("files "),
        "no members, no token — `files main` would duplicate this row's own state verdict:\n{text}"
    );
}

// ---------------------------------------------------------------------------
// F38 — `np embed` asks the origin for a missing pinned artifact. Socket-level proof.
//
// `cli.rs` can only prove *wording* against an address where nothing listens. This file owns the stub origin, so it owns
// the two claims wording cannot make: that a permitted fetch **opened a connection** (a request landed in the log), and
// that an origin which **answered** was treated as an answer rather than skipped as an outage — the distinction F38 §5's
// table turns on, and one no dead-port test can express.
// ---------------------------------------------------------------------------

/// `[embeddings]` pointing at a pinned id, so `np embed` in these cases wants a real local model. The block is written by
/// hand rather than through the crate: this is an end-to-end claim about what the binary does with its configuration.
#[cfg(all(feature = "local-model", feature = "fetch"))]
fn embeddings_config(id: &str) -> String {
    format!("[embeddings]\nprovider = \"local-model\"\nmodel = \"{id}\"\n")
}

/// A permitted fetch dials out — and an answered 404 is refused as an answer, never reported as offline.
#[cfg(all(feature = "local-model", feature = "fetch"))]
#[test]
fn embed_dials_out_for_a_missing_pinned_model_and_keeps_an_answer_an_answer() {
    let ws = Scratch::new();
    ws.write_user_config(&embeddings_config("embeddinggemma-2"));
    let origin = Origin::serve();

    let mut cmd = ws.command(&["embed"]);
    cmd.arg(&ws.root)
        .env("NP_MODELS_BASE_URL", &origin.base_url);
    let failed = run(&mut cmd);

    assert_eq!(failed.code(), 1, "\n{}", failed.stdout());
    let requests = origin.requests();
    assert!(
        !requests.is_empty(),
        "a missing pinned artifact means a request to the origin; the log stayed empty:\n{}",
        failed.combined()
    );
    assert!(
        requests.iter().any(|r| r.starts_with("GET ")),
        "the request is a GET on the pinned member: {requests:?}"
    );

    // The distinction this whole file exists for. An origin that answered 404 is a *refusal*: the run must not call itself
    // offline, because "connect and retry" is the wrong remedy for a mirror that has nothing to give.
    let text = failed.combined();
    assert!(
        !text.contains("offline"),
        "an answered 404 is not an outage: {text}"
    );
    assert!(
        text.contains("not found"),
        "`fetch`'s own class reaches the user, unchanged: {text}"
    );
    // Mutation this pins: routing every fetch error through `FetchStep::Offline`, which turns a broken mirror into a
    // permanent "offline" and hides the 404 behind a cable nobody has to plug in.
}

/// An id that is not pinned never reaches a transport — the request log stays empty, whatever `[embeddings]` says.
#[cfg(all(feature = "local-model", feature = "fetch"))]
#[test]
fn an_unpinned_id_never_opens_a_connection_during_embed() {
    let ws = Scratch::new();
    ws.write_user_config(&embeddings_config("some-other-model"));
    let origin = Origin::serve();

    let mut cmd = ws.command(&["embed"]);
    cmd.arg(&ws.root)
        .env("NP_MODELS_BASE_URL", &origin.base_url);
    let failed = run(&mut cmd);

    assert_eq!(failed.code(), 1, "\n{}", failed.stdout());
    // `requests` consumes the origin (it joins the worker to flush the log), so it is taken once and reused.
    let requests = origin.requests();
    assert!(
        requests.is_empty(),
        "the pinned set is checked before a transport is chosen: {requests:?}"
    );
    let text = failed.combined();
    // The wording belongs to `embed`'s configuration arm (no `models` row for an id this build cannot fetch), and
    // `cli.rs` pins that sentence; what this case owns is that the refusal happened **without a request** — so the exact
    // phrasing only has to name the id, and must not blame the network.
    assert!(
        text.contains("some-other-model"),
        "the refusal names the id it could not resolve: {text}"
    );
    assert!(
        !text.contains("offline"),
        "an unpinned id is a choice this build cannot make, never an outage: {text}"
    );
}

/// `np search` is the read path (**F38** §5): no fetch, even with everything configured to allow one and every reason to
/// want a model. The empty log is the whole point — a query that answered in milliseconds would otherwise be
/// indistinguishable from one that downloaded 310 MB first.
///
/// `--mode semantic` is what makes this falsifiable rather than vacuous. Hybrid mode on a vector-less workspace answers
/// from FTS5 and never resolves a provider at all (F36), so an empty log there would prove nothing about the fetch flag;
/// semantic mode has to resolve one, which is exactly where a search that fetched would dial out first.
#[cfg(all(feature = "local-model", feature = "fetch"))]
#[test]
fn search_never_dials_out_even_when_a_fetch_would_be_permitted() {
    let ws = Scratch::new();
    ws.write_user_config(&embeddings_config("embeddinggemma-2"));
    let origin = Origin::serve();

    let mut cmd = ws.command(&["search"]);
    cmd.arg(&ws.root)
        .arg("anything")
        .arg("--mode")
        .arg("semantic")
        .env("NP_MODELS_BASE_URL", &origin.base_url);
    let failed = run(&mut cmd);

    assert_eq!(
        failed.code(),
        1,
        "semantic mode has nothing to rank with:\n{}",
        failed.stdout()
    );
    let requests = origin.requests();
    assert!(
        requests.is_empty(),
        "search asked an origin for a model: {requests:?}"
    );
    let text = failed.combined();
    assert!(
        !text.contains("offline"),
        "a read command that cannot embed says the artifact is absent, not that the network failed: {text}"
    );

    // Mutation this pins: passing `true` for auto-fetch in `search`. That mutant leaves this log non-empty — a GET before
    // the refusal — which is the difference between "your query took four milliseconds" and "your query took four minutes
    // on a metered connection".
}

// ---------------------------------------------------------------- F42 option A: a pinned id that disagrees with its pin says so
//
// The state is not corruption and not refused: `np models install` exists for bytes a person holds, so it records the declaration it is
// handed. What it used not to do is *mention* that the declaration differs from what this build pins for that id — prefixes change vector
// bytes, so the workspace silently stops being comparable with anything `np models fetch` would install, and both honest commands then refuse
// to change the row (`install` will not rewrite an installed identity; `fetch` returns `IdentityDisagrees` before transferring a byte). The
// three cases below are one per direction the flag can point, because a mutant that always warns and a mutant that never warns each fail one
// of them:
//
//   * pinned id + a prefix the pin does not declare → warning + `prefix_drift`, naming which half;
//   * pinned id + flags omitted → **silent**: this build's pin declares empty prefixes, so omitting them is agreement. (This is the case
//     round four's Windows log recorded; the F42 card claimed it was drift and has been corrected.)
//   * unpinned id + prefixes supplied → silent: there is no pin to disagree with, and warning here would tell every hand-built model that it
//     is wrong.

/// The pinned id of this build, spelled locally rather than importing the pin table: a test that reads its expectation from the same constant
/// it is checking cannot notice the day the pin grows a prefix.
const PINNED_ID: &str = "embeddinggemma-2";

fn install_pinned(ws: &Scratch, source: &Path, digest: &str, extra: &[&str]) -> Command {
    let mut cmd = ws.models(&["install", "--id", PINNED_ID]);
    cmd.arg("--path")
        .arg(source)
        .arg("--dim")
        .arg("768")
        .arg("--sha256")
        .arg(digest);
    for flag in extra {
        cmd.arg(flag);
    }
    cmd
}

#[test]
fn installing_a_pinned_model_with_an_undeclared_prefix_says_so() {
    let ws = Scratch::new();
    let source = ws.source("artifact.bin", b"bytes with a meaning of their own\n");
    let digest = source_digest(&source);

    let installed = run(&mut install_pinned(
        &ws,
        &source,
        &digest,
        &[
            "--prefix-doc",
            "represent this sentence for retrieval: ",
            "--json",
        ],
    ));
    installed.assert_success();

    // The line belongs on stderr in both channels: stdout stays the one word (or the one document) it always was, and a person piping
    // stdout to a file still reads the warning.
    let err = installed.stderr();
    assert!(
        err.contains(PINNED_ID) && err.contains("document prefix"),
        "the notice names the id and which half drifted: {err}"
    );
    assert!(
        err.contains("np models list"),
        "and points at the command that shows what is on file, because there is no command that fixes it (F42 option B would): {err}"
    );

    let doc: serde_json::Value =
        serde_json::from_str(&installed.stdout()).expect("one JSON object");
    assert_eq!(
        doc["state"], "installed",
        "the install still succeeded (**A**, not a refusal)"
    );
    // Each half asserted on its own line: one comparison against a whole object would pass on `{doc:true,query:true}` for the wrong reason.
    assert_eq!(
        doc["prefix_drift"]["doc"], true,
        "the document prefix differs from the pin — stored vectors mean something else"
    );
    assert_eq!(
        doc["prefix_drift"]["query"], false,
        "and the query half is reported as matching, because it was omitted and the pin's is empty too"
    );
}

/// The agreement case, asserted because it is the one that keeps the warning honest. Both flags omitted records empty prefixes, and this
/// build's pin declares empty prefixes — so nothing may be printed. A mutant that warns on every pinned install passes the test above and
/// fails this one, which is exactly how a useful warning becomes noise nobody reads.
#[test]
fn installing_a_pinned_model_without_prefixes_stays_silent() {
    let ws = Scratch::new();
    let source = ws.source("artifact.bin", b"bytes that match the pin\n");
    let digest = source_digest(&source);

    let installed = run(&mut install_pinned(&ws, &source, &digest, &["--json"]));
    installed.assert_success();
    assert_eq!(
        installed.stderr(),
        "",
        "omitting both flags agrees with the pin; a warning here would be crying wolf"
    );
    let doc: serde_json::Value = serde_json::from_str(&installed.stdout()).expect("JSON");
    assert!(
        doc.get("prefix_drift").is_none(),
        "and the key is absent rather than null-or-false, so its presence means something: {doc}"
    );
}

#[test]
fn an_unpinned_id_is_never_told_it_disagrees_with_a_pin() {
    let ws = Scratch::new();
    let source = ws.source("artifact.bin", b"a model this build knows nothing about\n");
    let digest = source_digest(&source);

    // `Scratch::install` uses the fixture's own unpinned id.
    let installed = run(&mut {
        let mut cmd = ws.install(&source, "384", &digest);
        cmd.arg("--prefix-doc")
            .arg("anything at all: ")
            .arg("--json");
        cmd
    });
    installed.assert_success();
    assert_eq!(
        installed.stderr(),
        "",
        "no pin, nothing to disagree with — a warning would tell every hand-built model it is wrong"
    );
    let doc: serde_json::Value = serde_json::from_str(&installed.stdout()).expect("JSON");
    assert!(doc.get("prefix_drift").is_none(), "{doc}");
}

/// `[embeddings] model` naming `id`, and nothing else — no provider, no `[models]` key, no fetch of any kind.
///
/// The F38 section's own [`embeddings_config`] also sets `provider = "local-model"`, which is what a *fetch* case needs
/// and what this question does not: **"what would a download cost" is asked before any provider is chosen**, so the
/// listing has to answer it in a default build with `provider = "none"` too. Keeping the two helpers apart is what keeps
/// that claim checkable on the rows without the runtime.
/// Write `[embeddings] model = "<id>"` into this scratch's own config dir, and nothing else — no provider, no
/// `[models]` key, no fetch of any kind.
///
/// Deliberately **not** [`Scratch::write_user_config`], which lives in the `#[cfg(feature = "fetch")]` section with its
/// callers: **"what would a download cost" is asked before any provider is chosen and before any transport is compiled
/// in**, so the listing has to answer it on the `--no-default-features` row too. A helper gated like the fetch cases
/// would have made this key's coverage disappear exactly where the question still has an answer.
fn name_a_pinned_model(ws: &Scratch, id: &str) {
    let dir = ws.config_dir().join("note-processor");
    fs::create_dir_all(&dir).expect("test setup: config package dir");
    fs::write(
        dir.join("config.toml"),
        format!("[embeddings]\nmodel = \"{id}\"\n"),
    )
    .expect("test setup: write user config");
}

/// **How much would this cost, before it costs it?** (issue **#3**)
///
/// The app that reported it needs one sentence before the first embed — *"this needs `<id>`, about N MB, into
/// `<data_dir>`"* — and could name the id and the directory but not N. Every alternative is worse: a number hardcoded
/// client-side rots the moment the pin moves, and starting the transfer to learn its length is precisely the act that
/// needs consent first. `np/progress` already carries the pinned size mid-`fetching`; what was missing was asking
/// **before** anything starts.
///
/// The number is the build's own plan (`pins::plan_size`), which is the same sum the terminal consent prompt prints —
/// so a GUI and an `np setup` on one machine cannot quote two different prices for one download. A text-only build
/// plans the primary artifact; a `vision` build plans both halves, and this case reads whichever this binary would
/// actually transfer rather than asserting a constant that would be wrong on the other feature row.
#[test]
fn pinned_reports_what_a_fetch_would_cost_before_anything_starts() {
    let ws = Scratch::new();
    name_a_pinned_model(&ws, "embeddinggemma-2");

    let listed = run(&mut ws.list(true));
    listed.assert_success();
    let report = listed.json();

    let pinned = report["pinned"]
        .as_array()
        .cloned()
        .unwrap_or_else(|| panic!("`pinned` is an array on every state: {report}"));
    assert_eq!(pinned.len(), 1, "one configured id, one plan: {pinned:?}");
    assert_eq!(keys(&pinned[0]), PINNED_KEYS, "{pinned:?}");
    assert_eq!(pinned[0]["id"], "embeddinggemma-2", "{pinned:?}");

    let expected = np_core::pins::plan_size("embeddinggemma-2");
    assert_eq!(
        pinned[0]["bytes_total"].as_u64(),
        Some(expected),
        "the document's number is the pin table's own, not a transport announcement: {pinned:?}"
    );
    assert!(expected > 0, "a plan with no bytes is a bug in pins.rs");
    let members: Vec<&str> = pinned[0]["members"]
        .as_array()
        .expect("members is an array")
        .iter()
        .map(|name| name.as_str().expect("strings"))
        .collect();
    assert_eq!(
        members,
        np_core::pins::members_for_build(),
        "the plan names the files it adds up, in the order a fetch writes them"
    );

    // **Nothing installed, nothing downloaded.** The whole point is that this answer arrives with no artifact on disk:
    // `models` stays empty while `pinned` already prices the transfer.
    assert_eq!(report["models"].as_array().map(Vec::len), Some(0));
    assert_eq!(
        report["fetching"].as_array().map(Vec::len),
        Some(0),
        "and no fetch is in flight: {report}"
    );

    // **A workspace that names no pinned id gets an empty list, not a guess.** Two shapes of that, both from the
    // reporter's own rule ("empty when the workspace names no pinned id"): no `[embeddings]` section at all, and one
    // naming an id this build knows nothing about — which must not borrow some other model's number.
    let unconfigured = Scratch::new();
    let bare = run(&mut unconfigured.list(true));
    bare.assert_success();
    assert_eq!(
        bare.json()["pinned"],
        serde_json::Value::Array(Vec::new()),
        "no configured model is not a free download"
    );

    let unpinned = Scratch::new();
    name_a_pinned_model(&unpinned, "some-other-model");
    let stranger = run(&mut unpinned.list(true));
    stranger.assert_success();
    assert_eq!(
        stranger.json()["pinned"],
        serde_json::Value::Array(Vec::new()),
        "an id this build does not pin has no size to report, and inventing one is the lie this key exists to avoid"
    );

    // Mutation this catches: reporting `total_size` (every pinned member) instead of the build's plan fails the
    // `bytes_total` assertion on every non-vision row by 554,821,056 bytes; dropping the pin lookup and quoting a
    // literal fails the unpinned-id case.
}

/// The size question does not depend on the artifact directory being readable, so the directory-level failure keeps
/// answering it: `pinned` comes from config and the pin table, and a person whose models directory is unreadable is
/// exactly the person wondering what a re-fetch would cost.
#[test]
fn pinned_survives_the_directory_level_failure() {
    let ws = Scratch::new();
    name_a_pinned_model(&ws, "embeddinggemma-2");
    // The failure §3 knows: the models path exists as a *file*, so listing it is an error and `list` reports
    // `models_dir_error` with no rows at all.
    let models = ws.models_dir();
    // The `workspaces/` parent has to exist for the *path* to accept a file: nothing here has run a command yet, and
    // `models list` creates directories only when it is allowed to get that far.
    fs::create_dir_all(
        models
            .parent()
            .expect("test setup: the models dir sits inside `workspaces/`"),
    )
    .expect("test setup: create the workspaces dir");
    fs::write(&models, b"not a directory").expect("test setup: make the models path a file");

    let listed = run(&mut ws.list(true));
    assert_eq!(
        listed.code(),
        1,
        "the directory failure still refuses: {}",
        listed.stderr()
    );
    let report = listed.json();
    assert_eq!(keys(&report), LIST_KEYS, "{report}");
    assert!(
        !report["models_dir_error"].is_null(),
        "the failure is reported as it was: {report}"
    );
    assert_eq!(
        report["pinned"][0]["bytes_total"].as_u64(),
        Some(np_core::pins::plan_size("embeddinggemma-2")),
        "a question about bytes does not stop being answerable because a directory is broken: {report}"
    );
}
