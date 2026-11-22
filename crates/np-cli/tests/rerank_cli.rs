//! The rerank **doors**: `--rerank` / `--no-rerank` / `--rerank-text`, `[search] rerank*`, and what each failure costs at the
//! exit code (**F51**).
//!
//! These cases run against a real `np` binary in an isolated data dir, so they pin the two things a unit test cannot: that the
//! document shape is stable when nothing was asked for, and that a decline leaves the *answer* intact while saying so. Most of
//! them need no artifact at all — which is exactly the state most machines will be in, and the state whose behaviour matters
//! most. The one case that scores anything is gated on `NP_TEST_RERANK_PATH`, because the only bytes that pass verification are
//! 635 MB of them.

use std::fs;
use std::path::PathBuf;
use std::process::Output;

use tempfile::TempDir;

/// A vault with two notes, a private data dir and a private config dir — nothing on the developer's machine is read or written.
struct Vault {
    _tmp: TempDir,
    root: PathBuf,
    data: PathBuf,
    config: PathBuf,
}

impl Vault {
    fn new(search_toml: &str) -> Self {
        let tmp = TempDir::new().expect("test setup: tempdir");
        let root = tmp.path().join("ws");
        fs::create_dir_all(&root).expect("test setup: vault dir");
        // Two notes that both match `kafka`, so a rerank has something to reorder even with flat scores.
        for (name, body) in [
            ("a.md", "kafka broker tuning"),
            ("b.md", "kafka producer batching"),
        ] {
            fs::write(root.join(name), format!("# {name}\n\n{body}\n")).expect("test setup: note");
        }
        let data = tmp.path().join("data");
        let config = tmp.path().join("config");
        // The nested `note-processor/` directory is where the binary looks (`config_file_path`); a file written beside it is
        // read by nothing, which would make every `[search]` key below silently default.
        let package = config.join("note-processor");
        fs::create_dir_all(&package).expect("test setup: config dir");
        fs::write(
            package.join("config.toml"),
            format!(
                "[search]\n{search_toml}\n[embeddings]\nprovider = \"none\"\n",
                search_toml = search_toml.trim_end()
            ),
        )
        .expect("test setup: config");
        let vault = Self {
            _tmp: tmp,
            root,
            data,
            config,
        };
        // `np scan` first, so every case below searches an indexed vault. The operand is spelled here rather than in
        // [`Vault::command`] because `search` needs two of them and this one takes exactly the vault.
        let scanned = Run::of(
            vault
                .command(&["scan"])
                .arg(&vault.root)
                .output()
                .expect("np runs"),
        );
        scanned.assert_success();
        vault
    }

    fn command(&self, args: &[&str]) -> std::process::Command {
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_np"));
        cmd.env("XDG_DATA_HOME", &self.data)
            .env("LOCALAPPDATA", &self.data)
            .env("HOME", self._tmp.path())
            .env("XDG_CONFIG_HOME", &self.config)
            .env("APPDATA", &self.config)
            // A developer's shell must not decide whether these cases fetched, nor from where.
            .env_remove("NP_MODELS_BASE_URL")
            .env_remove("NP_MODELS_SHA256")
            .env_remove("NP_NO_AUTO_FETCH");
        cmd.args(args);
        cmd
    }

    /// `np search <vault> <query> [flags]`.
    fn search(&self, extra: &[&str]) -> std::process::Command {
        let mut cmd = self.command(&["search"]);
        cmd.arg(&self.root).arg("kafka");
        cmd.args(extra);
        cmd
    }

    /// Where `np models install` would place the reranker for this vault: the `.models` directory sharing its stem with the
    /// workspace database. Derived from the `.sqlite` beside it rather than re-deriving the key — a test that reimplemented the
    /// hash would happily assert about a directory np never uses.
    fn models_dir(&self) -> PathBuf {
        let workspaces = self.data.join("workspaces");
        let dbs: Vec<PathBuf> = fs::read_dir(&workspaces)
            .unwrap_or_else(|err| panic!("cannot read {}: {err}", workspaces.display()))
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.extension().is_some_and(|ext| ext == "sqlite"))
            .collect();
        assert_eq!(dbs.len(), 1, "exactly one workspace database");
        dbs[0].with_extension("models")
    }
}

struct Run {
    output: Output,
}

impl Run {
    fn of(output: Output) -> Self {
        Self { output }
    }
    fn code(&self) -> i32 {
        self.output
            .status
            .code()
            .unwrap_or_else(|| panic!("np was killed by a signal"))
    }
    fn stdout(&self) -> String {
        String::from_utf8_lossy(&self.output.stdout).into_owned()
    }
    fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.output.stderr).into_owned()
    }
    fn assert_success(&self) {
        assert_eq!(self.code(), 0, "stderr: {}", self.stderr());
    }
    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.stdout()).unwrap_or_else(|err| {
            panic!("stdout is one JSON object: {err}\n{}", self.stdout());
        })
    }
}

/// **The default run says `disabled`, in a shape that never moves.** This is the frozen-output fence for F51: a key appearing,
/// disappearing or renaming on a run where nobody asked for reranking would break every reader written against 0.6 without
/// warning. Note what is *not* here: no error, no non-zero exit, no stderr line — an off stage is the ordinary state.
#[test]
fn search_json_reports_a_rerank_stage_that_was_never_asked() {
    let vault = Vault::new("");

    let ok = Run::of(vault.search(&["--json"]).output().expect("np runs"));
    ok.assert_success();
    assert_eq!(
        ok.stderr(),
        "",
        "nothing was declined, so nothing may say so"
    );

    let document = ok.json();
    let rerank = &document["rerank"];
    let mut keys: Vec<String> = rerank
        .as_object()
        .expect("`rerank` is an object on every run")
        .keys()
        .cloned()
        .collect();
    keys.sort();
    assert_eq!(
        keys,
        ["candidates", "model", "note", "ran", "reason", "text"]
            .iter()
            .map(|key| (*key).to_string())
            .collect::<Vec<String>>(),
        "{document}"
    );
    assert_eq!(rerank["ran"], false);
    assert_eq!(rerank["reason"], "disabled");
    assert!(rerank["note"].is_null());
    assert_eq!(rerank["candidates"], 0);
    // `model` is null because no stage was asked for: naming an artifact the run never touched would read as though it had been
    // loaded and found wanting.
    assert!(rerank["model"].is_null());
    assert_eq!(
        rerank["text"], "title-body",
        "the policy that would apply, so a measurement can record it"
    );

    let hits = document["hits"].as_array().expect("hits is an array");
    assert!(!hits.is_empty(), "the keyword list answers");
    for hit in hits {
        assert!(
            hit["rerank_score"].is_null()
                && hit["rerank_rank_before"].is_null()
                && hit["rerank_rank_after"].is_null(),
            "an unscored hit carries no rerank evidence: {hit}"
        );
    }
}

/// **Configuration asks politely.** With `[search] rerank = true` and no artifact on disk the run answers in retrieval order,
/// exits 0, and prints one sentence naming the fix. Two refusals would be wrong here: the answer is real, and the human set the
/// key on a machine they expected to work.
///
/// The same run also proves `np search` never fetches: `[models] auto_fetch = true` is set, nothing was downloaded, and the
/// artifact directory holds no reranker file — so the promise in `docs/reference-models.md` is checked rather than asserted.
#[test]
fn rerank_by_config_with_no_artifact_answers_warns_and_exits_zero() {
    let vault = Vault::new("rerank = true\n\n[models]\nauto_fetch = true\n");

    let ok = Run::of(vault.search(&["--json"]).output().expect("np runs"));
    ok.assert_success();

    let document = ok.json();
    assert_eq!(document["rerank"]["ran"], false);
    // Which cause is reported depends on the build shape: a `local-model` binary looks at the disk and finds no artifact; one
    // without the runtime never gets that far, because this executable could not load any ranker. Both are declines, both exit
    // 0, and each names its own remedy below.
    let expected = if cfg!(feature = "local-model") {
        "rerank-artifact-missing"
    } else {
        "rerank-runtime-not-built"
    };
    assert_eq!(document["rerank"]["reason"], expected);
    assert_eq!(document["rerank"]["model"], np_core::pins::RERANKER_ID);
    assert!(!document["hits"].as_array().expect("hits").is_empty());

    // The machine channel carries the cause; the prose channel is where the sentence belongs, and `--json` stays parseable —
    // the same split every other search note keeps. So the wording is asserted on a second run without `--json`.
    let prose = Run::of(vault.search(&[]).output().expect("np runs"));
    prose.assert_success();
    assert!(
        !prose.stdout().contains("rerank"),
        "the decline is not a hit line; stdout stays the list:\n{}",
        prose.stdout()
    );
    let err = prose.stderr();
    if cfg!(feature = "local-model") {
        assert!(
            err.contains(np_core::pins::RERANKER_ID) && err.contains("np models install"),
            "the sentence names the artifact and the command that places it:\n{err}"
        );
    } else {
        assert!(
            err.contains("`local-model`"),
            "a build without the runtime says so, rather than sending the reader to install a file:\n{err}"
        );
    }

    let models = vault.models_dir();
    let placed: Vec<String> = match fs::read_dir(&models) {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(np_core::pins::RERANKER_ID))
            .collect(),
        // No directory at all, which is the strongest form of the claim: no transfer, no artifact, and nothing created on the
        // way to saying "not installed".
        Err(_) => Vec::new(),
    };
    assert!(
        placed.is_empty(),
        "a search must not fetch 635 MB because a ranking preference was set: {placed:?}"
    );
}

/// **A flag states an expectation**, so the same missing artifact is exit 1 with the cause on stderr and nothing on stdout. A
/// script that asked for reranked results and got retrieval order would otherwise report a measurement that never happened —
/// which is the one outcome worse than a refusal.
#[test]
fn explicit_rerank_without_the_artifact_refuses_at_exit_one() {
    let vault = Vault::new("");

    let refused = Run::of(
        vault
            .search(&["--rerank", "--json"])
            .output()
            .expect("np runs"),
    );
    assert_eq!(refused.code(), 1, "stderr:\n{}", refused.stderr());
    assert_eq!(refused.stdout(), "", "a refusal writes no partial document");
    let err = refused.stderr();
    // The cause differs by build shape — a missing artifact in a `local-model` build, this binary in one without the runtime —
    // and each sentence has to name *its own* fix: 635 MB of install versus a rebuild with the feature. Naming neither would
    // send a reader to look at their disk for a problem that lives in the executable.
    assert!(
        err.contains("rerank was requested"),
        "the refusal says what was asked for:\n{err}"
    );
    assert!(
        (err.contains(np_core::pins::RERANKER_ID) && err.contains("np models install"))
            || err.contains("`local-model`"),
        "and which of the two causes it is:\n{err}"
    );
}

/// **`--no-rerank` overrides a config that enabled it, silently.** That direction carries no warning: the human asked for one
/// run without the stage and got exactly that, so a stderr line would be noise on the path someone uses to compare the two.
#[test]
fn no_rerank_overrides_a_config_that_enabled_it() {
    let vault = Vault::new("rerank = true\n");

    let ok = Run::of(
        vault
            .search(&["--no-rerank", "--json"])
            .output()
            .expect("np runs"),
    );
    ok.assert_success();
    assert_eq!(ok.json()["rerank"]["reason"], "disabled");
    assert_eq!(
        ok.stderr(),
        "",
        "the run did what was asked, so it stays quiet"
    );
}

/// **A third spelling of `--rerank-text` is refused by clap, naming both accepted words** — the same two words a config key
/// names, because one `FromStr` backs both doors. This is the value that decides which bytes the model reads, so a typo silently
/// becoming the default would change an answer with no diagnosis at all.
#[test]
fn rerank_text_refuses_a_third_spelling_naming_two() {
    let vault = Vault::new("");

    let refused = Run::of(
        vault
            .search(&["--rerank-text", "titles"])
            .output()
            .expect("np runs"),
    );
    assert_eq!(
        refused.code(),
        2,
        "clap's usage error; stderr:\n{}",
        refused.stderr()
    );
    let err = refused.stderr();
    for word in ["title-body", "body"] {
        assert!(err.contains(word), "`{word}` must be offered:\n{err}");
    }
    assert!(err.contains("titles"), "and the value it was given:\n{err}");
}

/// **The stage actually reorders real bytes**, and reports what it scored. Needs both the runtime and the pinned artifact, so it
/// is compiled only into a `local-model` build and run only when `NP_TEST_RERANK_PATH` names the file: the digest check refuses
/// anything else, and inventing bytes to satisfy it would test the check rather than the stage.
///
/// The claim under test is the depth rule end to end: `--limit 1` with `rerank_top_n = 2` scores **two** candidates and prints
/// **one**. That pair is what a truncated-first-then-rerank implementation gets wrong, and it is only visible against a scorer
/// that really ran.
#[cfg(feature = "local-model")]
#[test]
fn rerank_scores_more_candidates_than_it_prints() {
    let Some(source) = std::env::var_os("NP_TEST_RERANK_PATH").map(PathBuf::from) else {
        println!(
            "NOT COVERAGE: NP_TEST_RERANK_PATH is unset, so no real cross-encoder scored anything here. Re-run with it \
             pointing at bge-reranker-v2-m3 (sha256 d32a877b…) to exercise the stage."
        );
        return;
    };
    let vault = Vault::new("rerank_top_n = 2\n");
    let models = vault.models_dir();
    fs::create_dir_all(&models).expect("test setup: models dir");
    fs::copy(&source, models.join(np_core::pins::RERANKER_ID)).expect("test setup: place artifact");

    let ok = Run::of(
        vault
            .search(&["--rerank", "--limit", "1", "--json"])
            .output()
            .expect("np runs"),
    );
    assert_eq!(ok.code(), 0, "stderr:\n{}", ok.stderr());
    let document = ok.json();
    assert_eq!(document["rerank"]["ran"], true);
    assert_eq!(document["rerank"]["candidates"], 2, "two scored");
    assert_eq!(
        document["hits"].as_array().expect("hits").len(),
        1,
        "one printed"
    );
    let hit = &document["hits"][0];
    assert!(
        hit["rerank_score"].is_number() && hit["rerank_rank_before"].is_number(),
        "the printed hit carries the evidence of a real scoring pass: {hit}"
    );
}

/// The shape without the runtime, pinned by the default-feature build this file otherwise runs in: **config declines with a
/// warning, the flag refuses** — one cause, two policies, decided by who asked rather than by what is on disk.
#[cfg(not(feature = "local-model"))]
#[test]
fn a_build_without_the_runtime_declines_by_config_and_refuses_by_flag() {
    let vault = Vault::new("rerank = true\n");

    let by_config = Run::of(vault.search(&["--json"]).output().expect("np runs"));
    by_config.assert_success();
    assert_eq!(
        by_config.json()["rerank"]["reason"],
        "rerank-runtime-not-built"
    );

    let by_flag = Run::of(vault.search(&["--rerank"]).output().expect("np runs"));
    assert_eq!(by_flag.code(), 1);
    assert!(
        by_flag.stderr().contains("local-model"),
        "the flag's refusal has to name the missing feature, not a missing file:\n{}",
        by_flag.stderr()
    );
}
