//! Builds and links the pinned llama.cpp into `np-core` — under `local-model`, and only there.
//!
//! Design authority: `docs/design-m2-local-embeddings.md` §14 (we own this build; no bindgen, no
//! pinned Rust binding) and §2's promise that a default `cargo test --workspace` never grows a C++
//! build. So the first thing this script does is check whether it was asked to exist at all:
//! with the feature off it prints one `rerun-if-changed` line and touches nothing — no cmake probe,
//! no filesystem read beyond cargo's own. That is what makes "default build unchanged" a fact rather
//! than a hope (measured in the round report: identical stripped bytes with and without this file).
//!
//! With the feature on it does four things, in this order, and refuses loudly at each failure:
//!
//! 1. reads `third_party/llama-cpp.toml` — the pin, which is the only place the tag, the digest and
//!    the CMake options are written — then appends whatever the feature shape asks for ([`Shape`]);
//! 2. finds a source tree (`NP_LLAMA_CPP_SRC`, else `.vendor/llama.cpp-<tag>/`) and **verifies it is
//!    the pin**: the digest of `include/llama.h` (the file our FFI mirror was copied from), then the
//!    version numbers, then the architecture token — never a directory name;
//! 3. configures and builds the shape's cmake targets (`llama`, plus `mtmd` under `vision`);
//! 4. emits the link search paths, the shape's static archives, and the system libraries they need.
//!
//! ## The two shapes
//!
//! `local-model` builds one target and links four archives. `local-model,vision` adds one cmake option
//! (`LLAMA_BUILD_MTMD=ON`, which v0.8.0 defaults to off — that is why a text-only build has never compiled
//! `tools/mtmd`), one target (`mtmd`) and **two** archives: `libmtmd.a` needs `hash_sha256_hex`, which lives in
//! `libvendor-hash.a`. Measured, not inferred — `crates/np-core/tests/probes/vision-spike-2026-11-02.md` links six
//! archives and records the `undefined reference to hash_sha256_hex[abi:cxx11]` failure of the five-archive attempt.
//! The option joins the pin's list before the configure, so the machinery below still covers it: an option cmake filed
//! as `UNINITIALIZED` is a refusal either way, and an option set that changed still forces a fresh directory rather
//! than a reconfigure that cannot undo a deletion.
//!
//! Two rules it exists to enforce:
//!
//!   * **Never fetch.** The fetch belongs to `scripts/vendor-llama-cpp.sh`, which a human or the one
//!     CI job runs. A build script that quietly reaches out fails in ways we cannot reproduce (§14).
//!   * **Never silently disable.** Every failure here is a panic naming what was missing and the
//!     command that fixes it. A release that quietly loses inference is worse than a loud failure —
//!     and unlike `cargo:warning=`, a panic cannot be scrolled past.
//!
//! One limit worth knowing before you edit C++ to debug something. Cargo asks us to run when `build.rs`,
//! the pin file, or one of the three environment variables changes, and — since R6b0's review — when
//! `include/llama.h` or `CMakeLists.txt` in the resolved tree changes, because a hand-written mirror must
//! not drift from the header it mirrors. It is **not** re-run by an edit to some other `.cpp`, so such an
//! edit does not reach the archive we link: the stale `libllama.a` stays. Watch every file in a 214 MB tree
//! would cost a recursive fingerprint per feature-on cargo invocation; naming two files costs nothing and
//! covers the unsound half. After a C++ edit, `touch crates/np-core/build.rs`.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The pin file, relative to the workspace root. One copy of these values, two readers (the vendor
/// script reads `tag`/`url`/`sha256`; here we read `tag` and `[cmake]`).
const PIN: &str = "third_party/llama-cpp.toml";

/// The cmake target that produces everything a text-only build links, and the name a build directory inside
/// `OUT_DIR` carries. Building `install`, or the default target, would compile binaries `np` never contains;
/// [`Shape::targets`] is what says whether `mtmd` travels with it.
const TARGET: &str = "llama";

/// The extra target `vision` builds: llama.cpp's multimodal library. Its `CMakeLists.txt` is only read when
/// [`MTMD_CMAKE_OPTION`] is ON (`CMakeLists.txt:267-269` of the pinned tree).
const MTMD_TARGET: &str = "mtmd";

/// The cmake option that makes `tools/mtmd` exist. The pinned tree declares it as
/// `option(LLAMA_BUILD_MTMD "llama: build tools/mtmd library standalone" OFF)`, so it lands in the cache with a real
/// type and [`verify_options_understood`] can vouch for it.
const MTMD_CMAKE_OPTION: (&str, &str) = ("LLAMA_BUILD_MTMD", "ON");

/// `libmtmd.a`, first in link order because it is a dependent of `llama`.
const MTMD_ARCHIVE: &str = "mtmd";

/// `libvendor-hash.a`, last in link order: nothing depends on it and it is here only because `libmtmd.a` names
/// `hash_sha256_hex`. The target is `vendor-hash` (`vendor/hash/CMakeLists.txt:1`), which is the archive's file name,
/// not the `vendor::hash` alias `mtmd` links against.
const HASH_ARCHIVE: &str = "vendor-hash";

/// Static archives for a text-only build, in the order GNU ld resolves them — **dependents before dependencies**,
/// left to right: `llama` → `ggml` → `ggml-cpu` → `ggml-base`. [`Shape::archives`] puts [`MTMD_ARCHIVE`] in front of
/// this list and [`HASH_ARCHIVE`] behind it when the feature asks for vision.
///
/// The previous order (`llama, ggml, ggml-base, ggml-cpu`) put the pure dependency ahead of a dependent and linked
/// anyway, which meant ld had already pulled the base objects `libggml-cpu.a` needs — symbol-overlap luck, not a
/// property that survives a re-pin or a cmake layout change (A2 review P2-a). This order is measured, not reasoned
/// about: it links from a wiped build directory with `local_runtime`'s link proofs green. `ggml` and `ggml-cpu` do
/// reference each other (`libggml.a` names `ggml_backend_cpu_reg`; the reverse names `ggml_*`), so no ordering is
/// airtight without a linker group — what this removes is the case where everything that needs `ggml-base` sits behind it.
const ARCHIVES: [&str; 4] = ["llama", "ggml", "ggml-cpu", "ggml-base"];

/// Libraries the archives need from outside themselves, measured rather than guessed:
/// `nm --undefined-only` over the four archives left `__cxa_*`/`_Unwind_Resume` (libstdc++),
/// `exp`/`log`/`pow`/`sqrt` (libm), `pthread_*` (libpthread) and `dlopen`/`dlsym`/`dlclose` from
/// `ggml.c`'s dynamic-backend loader (libdl). `llama.pc` and `ggml-config.cmake` add nothing else for
/// this configuration, and the link of the version/system-info probe confirmed it.
const SYSTEM_LIBS: [&str; 4] = ["stdc++", "m", "pthread", "dl"];

/// The Windows set, and the difference is measured rather than translated: a `x86_64-w64-mingw32` link of
/// these same four archives resolved with `ws2_32` and `bcrypt`, needed no `pthread` (ggml's threading rides
/// the Win32 API under the win32 threading model) and no `dl`. `stdc++` stays because llama.cpp is C++.
/// Measured 2026-11-08 in the cross-build spike: `ADVAPI32`, `KERNEL32`, `msvcrt` and nothing else in the
/// import table of an exe that references `llama_model_load_from_file`.
const WINDOWS_SYSTEM_LIBS: [&str; 3] = ["m", "ws2_32", "bcrypt"];

/// Set this to a cmake toolchain file to build `local-model` for a target that is not the host. Its existence
/// is the whole opt-in: without it, a non-Linux target still meets the refusal below rather than a half-built
/// C++ tree. Read from a cross recipe (`docs/ops-cross-build-windows-gnu.md`), never guessed at here.
const TOOLCHAIN_ENV: &str = "NP_LLAMA_CMAKE_TOOLCHAIN";

/// Set this to build llama.cpp for **any** x86-64 machine rather than for the one running cmake. The release
/// scripts (`.github/scripts/package-assets.sh`, `.github/scripts/cross-build-windows.sh`) set it; a developer
/// building for their own machine leaves it unset and keeps upstream's `-march=native` fast path. Read once, then
/// enforced from cmake's cache by [`require_portable_codegen`] — see the measurement at the configure site for why
/// both halves are needed.
const PORTABLE_ENV: &str = "NP_LLAMA_PORTABLE";

/// The options that each raise the instruction-set floor of a static build. `GGML_NATIVE` alone is not the floor:
/// turning it off on a native build switches upstream's `INS_ENB` on, which turns **these** on, and they become
/// `-msse4.2 … -mavx2` flags on the whole cpu backend (`ggml/src/ggml-cpu/CMakeLists.txt:350-371`). AVX512/VNNI/AMX
/// default OFF in every mode, so they are not listed; if a future pin turns one on by default, add it here and the
/// cache check will say so.
const PORTABLE_ISA_OPTIONS: [&str; 6] = [
    "GGML_SSE42",
    "GGML_AVX",
    "GGML_AVX2",
    "GGML_FMA",
    "GGML_F16C",
    "GGML_BMI2",
];

/// The instruction-set floor a portable build compiles to, spelled the way the compiler spells it. Appended **last** to
/// whatever `CFLAGS`/`CXXFLAGS` already said, because gcc honours the rightmost `-march=`: on the release path this promise
/// outranks an inherited environment, and [`require_portable_codegen`] says so when it does not hold.
const PORTABLE_MARCH_FLAG: &str = "-march=x86-64";

/// What a baseline x86-64 compiler must **not** predefine. Read from the compiler rather than from a flag list, because the
/// flags were never the whole story: on 2026-11-13 a hosted runner's `cc` predefined `__AVX__` and `__AVX2__` from an empty
/// command line — its *default* target sits above the floor — which is how a tree with every `GGML_<ISA>` option OFF and no
/// `-m` token in any of its 76 compile commands still produced 36,203 wide-vector instructions.
const PORTABLE_FORBIDDEN_MACROS: [&str; 12] = [
    "__SSE3__",
    "__SSSE3__",
    "__SSE4_1__",
    "__SSE4_2__",
    "__AVX__",
    "__AVX2__",
    "__FMA__",
    "__F16C__",
    "__BMI__",
    "__BMI2__",
    "__POPCNT__",
    "__LZCNT__",
];

/// Where [`stage_prefixed_archives`] puts the archives a cross build links against.
const LINK_STAGE_DIR: &str = "llama-link-windows-gnu";

/// OpenMP is not in [`SYSTEM_LIBS`] because it is conditional: `libggml-cpu.a` and `libggml-base.a`
/// reference `GOMP_*` only when the tree was configured with OpenMP on. `ggml-config.cmake` gates its
/// own `OpenMP::OpenMP_C` interface on `GGML_OPENMP_ENABLED`, so we read that same cache entry rather
/// than hardcoding a second opinion — and a link error naming `GOMP_barrier` means this read is wrong.
const OPENMP_CACHE_KEY: &str = "GGML_OPENMP_ENABLED";
const OPENMP_LIB: &str = "gomp";

/// The architecture token that made v0.8.0 the pin (§13, §14): a tree without it cannot resolve
/// `general.architecture = gemma-embedding2`, whatever its version numbers say.
const REQUIRED_ARCH_TOKEN: &str = "LLM_ARCH_GEMMA_EMBEDDING2";

/// Ceiling on cmake's `-j`. Not a guess at the fastest build but at the largest safe one: §14 measured
/// 23 s at `-j 14` on a host with ~6.3 GB free RAM, and llama.cpp's translation units are large
/// enough that outrunning memory looks like a compiler being killed, not like a slow build. Override
/// with `NP_LLAMA_CPP_JOBS`.
const DEFAULT_JOB_CEILING: usize = 14;

fn main() {
    // Emitted first, and before anything is read: with the feature off this is the only line this
    // script prints and the only file it names.
    println!("cargo::rerun-if-changed=build.rs");

    if env::var_os("CARGO_FEATURE_LOCAL_MODEL").is_none() {
        return;
    }

    if let Err(problem) = link_pinned_llama(&Shape::from_features()) {
        panic!("{problem}");
    }
}

/// What the enabled cargo features ask this build to be: the text-only runtime, or that plus `mtmd`.
///
/// One type rather than a bool threaded through four call chains, because the three differences between the shapes —
/// an option, a target, two archives — have to stay in step. A build that configured `LLAMA_BUILD_MTMD=ON` and then
/// linked four archives fails at link time with a message about missing symbols instead of about features; a build
/// that built `mtmd` without being asked to pays for C++ nothing links.
struct Shape {
    vision: bool,
}

impl Shape {
    fn from_features() -> Self {
        // Cargo declares `vision = ["local-model"]`, so there is no shape with vision on and the runtime off; the
        // local-model check already happened in [`main`].
        Self {
            vision: env::var_os("CARGO_FEATURE_VISION").is_some(),
        }
    }

    /// cmake targets to build, in dependency order (`mtmd` links `llama`, so naming both is enough — and naming only
    /// `mtmd` would leave the archive this file's whole existence depends on unbuilt).
    fn targets(&self) -> &'static [&'static str] {
        if self.vision {
            &[TARGET, MTMD_TARGET]
        } else {
            &[TARGET]
        }
    }

    /// The `-D` entries this shape adds to the pin's `[cmake]` table. They are deliberately **not** written into
    /// `third_party/llama-cpp.toml`: that file records how every shape builds the tree, and an option only one shape
    /// wants belongs with the shape. [`Pin::read`] appends them, so the option stamp and the UNINITIALIZED check see
    /// them exactly as they see the pinned ones.
    fn extra_options(&self) -> &'static [(&'static str, &'static str)] {
        if self.vision {
            &[MTMD_CMAKE_OPTION]
        } else {
            &[]
        }
    }

    /// Archives in link order: dependents before dependencies (see [`ARCHIVES`] for the measurement that fixed the
    /// text-only order, and this file's header for why a vision link is six rather than five).
    fn archives(&self) -> Vec<&'static str> {
        if self.vision {
            vec![
                MTMD_ARCHIVE,
                ARCHIVES[0],
                ARCHIVES[1],
                ARCHIVES[2],
                ARCHIVES[3],
                HASH_ARCHIVE,
            ]
        } else {
            ARCHIVES.to_vec()
        }
    }

    /// Name for the build directory inside `OUT_DIR`, so two shapes on one disk are distinguishable. Cargo already
    /// gives each feature set its own `OUT_DIR`; this keeps the paths honest rather than adding a second cache.
    fn build_dir_name(&self) -> String {
        format!("llama-cpp-{}", self.targets().join("-"))
    }
}

/// Everything the feature-on path does, so one `Err(String)` becomes one honest panic.
fn link_pinned_llama(shape: &Shape) -> Result<(), String> {
    // Every variable read below gets a `rerun-if-env-changed`, so re-pointing the build at a
    // different tree or a different cmake is enough to make cargo ask us again.
    // `CC`/`CXX` are here because cmake caches the compiler it found first: switching compilers by env
    // without this line leaves a stale cache that cmake usually (not always) catches, and our own reuse rule
    // would happily link the archive set built by the other one.
    for name in [
        "NP_LLAMA_CPP_SRC",
        "CMAKE",
        "NP_LLAMA_CPP_JOBS",
        "CC",
        "CXX",
    ] {
        println!("cargo::rerun-if-env-changed={name}");
    }

    println!("cargo::rerun-if-env-changed={TOOLCHAIN_ENV}");
    println!("cargo::rerun-if-env-changed={PORTABLE_ENV}");

    // §14 claimed nothing about Windows or macOS, so this refused every non-Linux target: better a refusal
    // naming the design section than a stranger meeting `-l stdc++` cannot find (macOS) or four wrong archive
    // names (MSVC). That scope still stands for anything not spelled out below. What changed on 2026-11-08 is
    // that `windows` + `gnu` stopped being a claim and became a measurement: llama.cpp v0.8.0 configures and
    // builds with mingw-w64 from this Linux host, all 30 functions in `llama_ffi.rs` are defined by the
    // resulting archives, and an exe linked against them imports only system DLLs. It stays opt-in, because a
    // cross build of C++ must never happen as a side effect of someone typing `--target`.
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    let cross = match (target_os.as_str(), target_env.as_str()) {
        ("linux", _) => None,
        ("windows", "gnu") => match env::var_os(TOOLCHAIN_ENV) {
            Some(file) => {
                let path = PathBuf::from(&file);
                if !path.is_file() {
                    return Err(format!(
                        "{TOOLCHAIN_ENV}={file:?} is not a file. A cmake toolchain file is what makes this \n  \
                         cross build mean `windows-gnu`: point it at one that names the mingw-w64 compilers.",
                    ));
                }
                Some(path)
            }
            None => {
                return Err(format!(
                    "building `local-model` for windows-gnu needs {TOOLCHAIN_ENV} to name a cmake toolchain \n  \
                     file. Nothing is built without it: §14 measured this feature on Linux, and a Windows cross \n  \
                     build that happens by accident would put an unmeasured C++ tree into a release asset. See \n  \
                     docs/ops-cross-build-windows-gnu.md for the recipe this variable expects.",
                ))
            }
        },
        _ => {
            return Err(format!(
                "`local-model` is built for Linux and for windows-gnu (with {TOOLCHAIN_ENV}). This target is \n  \
                 {target_os:?}/{target_env:?}. design-m2-local-embeddings.md §14 keeps its rule: no platform claim \n  \
                 without a build measured on it — macOS has no `libstdc++` to name and MSVC wants different archive \n  \
                 names, so both still get a refusal rather than a linker error.",
            ))
        }
    };

    let manifest = path_env("CARGO_MANIFEST_DIR")?;
    // crates/np-core -> the workspace root, which is where `.vendor/` and `third_party/` live.
    let root = manifest
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| format!("{manifest:?} is not inside a workspace"))?
        .to_path_buf();
    let out_dir = path_env("OUT_DIR")?;

    let pin_path = root.join(PIN);
    println!("cargo::rerun-if-changed={}", pin_path.display());
    let pin = Pin::read(&pin_path, shape.extra_options())?;
    eprintln!(
        "np-core: building llama.cpp {} for `local-model{}` ({} CMake options from {}{}; targets {:?})",
        pin.tag,
        if shape.vision {
            ",vision"
        } else {
            ""
        },
        pin.options.len(),
        PIN,
        if shape.vision {
            ", plus the vision shape's LLAMA_BUILD_MTMD"
        } else {
            ""
        },
        shape.targets(),
    );

    let source = resolve_source(&root, &pin)?;

    // The two files in the resolved tree whose contents change what we may claim about it. Naming them is
    // also what makes a deleted tree loud: cargo re-runs this script when a named input is *missing*, so
    // `rm -rf .vendor/llama.cpp-<tag>` now reaches [`resolve_source`]'s refusal instead of linking the stale
    // archive left in `OUT_DIR` (measured before the fix: exit 0, nothing rebuilt, no message).
    for important in ["include/llama.h", "CMakeLists.txt"] {
        println!(
            "cargo::rerun-if-changed={}",
            source.join(important).display()
        );
    }

    check_that_this_is_the_pin(&source, &pin)?;

    let cmake = cmake_command()?;
    // A cross build gets its own directory and never reuses `<source>/build`: cmake caches the compiler it was
    // first configured with and refuses another, and `choose_build_dir` would hand a host cache to a windows
    // build the moment anyone had run the Linux feature once in that tree — an error naming neither cause nor cure.
    let build_dir = match &cross {
        Some(_) => out_dir.join(format!("{}-windows-gnu", shape.build_dir_name())),
        None => choose_build_dir(&source, &out_dir, shape),
    };
    configure(
        &cmake,
        &source,
        &build_dir,
        &out_dir,
        &pin,
        cross.as_deref(),
    )?;
    compile(&cmake, &build_dir, shape.targets())?;
    // `ggml/CMakeLists.txt:80` clears `CMAKE_STATIC_LIBRARY_PREFIX` on Windows for its own targets — measured
    // 2026-11-08: the build tree came out `libllama.a`, `ggml.a`, `ggml-cpu.a`, `ggml-base.a`, and no `-D` of
    // ours overrides a subdirectory's own `set()`. GNU ld will not look at `ggml.a` for `-l ggml` ("rename it to
    // libggml-base.a", it says), so the cross link gets prefixed copies in one directory of ours. Copies, not
    // renames: the cmake tree keeps owning its build directory, and a re-run of cmake below must find what it
    // expects to have built.
    let link_dir = match &cross {
        Some(_) => stage_prefixed_archives(&build_dir, &out_dir, shape)?,
        None => build_dir.clone(),
    };
    if cross.is_some() {
        // rustc only searches the `-L native=` paths we hand it, and the mingw sysroot is not one of them: the
        // first attempt failed with "could not find native static library `stdc++`" (measured 2026-11-08). Ask
        // the compiler that actually built the archives where its own runtime archives live, rather than
        // hard-coding a path that belongs to whoever installed this toolchain.
        let compiler = configured_compiler(&build_dir)
            .ok_or_else(|| format!("no compiler is recorded under {}", build_dir.display()))?;
        for dir in windows_runtime_search_dirs(Path::new(&compiler))? {
            println!("cargo::rustc-link-search=native={}", dir.display());
        }
    }
    emit_link_flags(&link_dir, shape, cross.is_some())
}

/// The C++ source to build: `NP_LLAMA_CPP_SRC` if set (offline work, CI caches, and the case where a
/// tree is already configured), else the directory `scripts/vendor-llama-cpp.sh` unpacks.
fn resolve_source(root: &Path, pin: &Pin) -> Result<PathBuf, String> {
    // `var_os`, not `var(...).unwrap_or_default()`: this file's rule is that it never silently disables itself, and the
    // old form turned a value cargo cannot decode as UTF-8 into "unset" — quietly building the vendored tree when a
    // different one was named (A2 review P2-8). A path does not need to be UTF-8 to be a path.
    let override_dir = env::var_os("NP_LLAMA_CPP_SRC");
    let trimmed = override_dir.as_deref().map(os_trim).unwrap_or_default();
    if !trimmed.is_empty() {
        let path = PathBuf::from(&trimmed);
        if path.is_dir() {
            return Ok(path);
        }
        return Err(format!(
            "NP_LLAMA_CPP_SRC={} is not a directory.\n  \
             Unset it to use the vendored tree, or point it at llama.cpp {} unpacked from the pinned tarball.",
            trimmed.to_string_lossy(),
            pin.tag
        ));
    }

    let vendored = root.join(".vendor").join(format!("llama.cpp-{}", pin.tag));
    if vendored.is_dir() {
        return Ok(vendored);
    }

    Err(format!(
        "no llama.cpp {tag} source tree to build `local-model` from.\n  \
         tried: {}\n  \
         This build script never downloads anything (§14: no network during `cargo build`). Either run\n  \
           ./scripts/vendor-llama-cpp.sh\n  \
         once, which verifies the sha256 recorded in {PIN}, or name a tree you already have:\n  \
           NP_LLAMA_CPP_SRC=/path/to/llama.cpp-{tag} cargo build -p np-core --features local-model",
        vendored.display(),
        tag = pin.tag
    ))
}

/// Verify the tree is what we pinned, by reading it. A directory called `llama.cpp-v0.8.0` proves
/// nothing; a version triple and an architecture token do.
fn check_that_this_is_the_pin(source: &Path, pin: &Pin) -> Result<(), String> {
    if !source.join("include").join("llama.h").is_file() {
        return Err(format!(
            "{} has no include/llama.h, so it is not a llama.cpp source tree.",
            source.display()
        ));
    }

    // The strongest and cheapest identity check first: one 70 KB read. Version numbers cannot tell v0.8.0
    // from any commit after it (`LLAMA_BUILD_IS_DEV` makes them all report "0.8.0-dev"), and only this digest
    // can tell the header we mirrored from a header that merely has the same version.
    let header = source.join("include").join("llama.h");
    let found_digest = sha256_hex(
        &fs::read(&header).map_err(|e| format!("cannot read {}: {e}", header.display()))?,
    );
    if found_digest != pin.header_sha256 {
        return Err(format!(
            "{} is not the header our FFI mirror was copied from.\
             \n  found:  {}\
             \n  pinned: {} ({PIN}: header_sha256)\
             \n  tree:   {}\
             \n`{PIN}` mirrors field-for-field one revision of `include/llama.h`; building a different one is \
             unsound memory with a green test suite. Re-pin deliberately (and re-check the mirror), or point \
             NP_LLAMA_CPP_SRC at the pinned tree.",
            header.display(),
            found_digest,
            pin.header_sha256,
            source.display()
        ));
    }

    let (want_major, want_minor, want_patch) = version_of_tag(&pin.tag)?;
    let cmake_lists = source.join("CMakeLists.txt");
    let text = read(&cmake_lists)?;
    for (field, want) in [
        ("MAJOR", want_major),
        ("MINOR", want_minor),
        ("PATCH", want_patch),
    ] {
        let found = cmake_set_value(&text, &format!("LLAMA_VERSION_{field}")).ok_or_else(|| {
            format!(
                "{} does not declare LLAMA_VERSION_{field}, so it is not the llama.cpp we pinned ({}).",
                cmake_lists.display(),
                pin.tag
            )
        })?;
        if found != want.to_string() {
            return Err(format!(
                "{} declares LLAMA_VERSION_{field} {found}, but {} pins {}.\n  \
                 Refusing to build a tree that is not the one whose sha256 we recorded.",
                cmake_lists.display(),
                pin.tag,
                PIN
            ));
        }
    }

    let arch = source.join("src").join("llama-arch.cpp");
    if !read(&arch)?.contains(REQUIRED_ARCH_TOKEN) {
        return Err(format!(
            "{} does not mention {REQUIRED_ARCH_TOKEN}. That token is why {} was pinned (§14): a tree \
             without it cannot resolve `general.architecture = gemma-embedding2`, so building it would \
             buy 10 MB of runtime that cannot run our model.",
            arch.display(),
            pin.tag
        ));
    }

    Ok(())
}

/// `v0.8.0` -> `(0, 8, 0)`. The tag is the version; there is no second place to read it from.
fn version_of_tag(tag: &str) -> Result<(u8, u8, u8), String> {
    let mut parts = tag.trim_start_matches('v').split('.');
    let mut next = |what: &str| -> Result<u8, String> {
        parts
            .next()
            .ok_or_else(|| format!("{PIN}: tag {tag} has no {what} number"))?
            .parse::<u8>()
            .map_err(|_| format!("{PIN}: tag {tag} has a non-numeric {what} part"))
    };
    let version = (next("major")?, next("minor")?, next("patch")?);
    if parts.next().is_some() {
        return Err(format!("{PIN}: tag {tag} is not a plain X.Y.Z tag"));
    }
    Ok(version)
}

/// Value of `set(<name> <value>)` in a CMakeLists file, or None when absent.
fn cmake_set_value(text: &str, name: &str) -> Option<String> {
    let prefix = format!("set({name}");
    let line = text
        .lines()
        .find(|line| line.trim_start().starts_with(&prefix))?;
    let after = line[line.find(&prefix)? + prefix.len()..].trim_start();
    Some(after.split([')', ' ', '\t']).next()?.to_string())
}

/// The cmake binary: `CMAKE` if set, else whatever is on PATH. Probed with `--version`, because the
/// failure we are trying to explain is "cmake is not installed", and cargo's own message for a failed
/// `Command::spawn` is `No such file or directory (os error 2)`.
fn cmake_command() -> Result<OsString, String> {
    // Same rule as `resolve_source`: an undecodable `CMAKE` must not read as "not set" and fall through to whatever is
    // on PATH (A2 review P2-8).
    let configured = env::var_os("CMAKE");
    let requested = configured.as_deref().map(os_trim).unwrap_or_default();
    let binary: OsString = if requested.is_empty() {
        OsString::from("cmake")
    } else {
        requested
    };

    let probe = Command::new(&binary).arg("--version").output();
    match probe {
        Ok(out) if out.status.success() => Ok(binary),
        Ok(out) => Err(format!(
            "`{:?} --version` failed: {}",
            binary,
            String::from_utf8_lossy(&out.stderr).trim()
        )),
        Err(error) => Err(format!(
            "cannot run cmake as {binary:?} ({error}).\n  \
             Point the build at it — cmake needs no root, a Kitware tarball unpacked anywhere works:\n  \
               CMAKE=$HOME/tools/cmake/bin/cmake cargo build -p np-core --features local-model\n  \
             (§14 measured this host with exactly that path.)"
        )),
    }
}

/// Reuse a build directory the source tree already has — that is how `NP_LLAMA_CPP_SRC` avoids
/// recompiling C++ on every cargo run — and otherwise build inside `OUT_DIR`, where cargo owns it.
///
/// An in-tree directory is shared by both shapes, which is one reason [`Shape::extra_options`] feeds the option stamp:
/// a cache configured without `LLAMA_BUILD_MTMD` cannot serve a vision build, and the stamp difference is what says so
/// — followed by [`configure`]'s refusal to delete a directory that is not ours.
fn choose_build_dir(source: &Path, out_dir: &Path, shape: &Shape) -> PathBuf {
    let in_tree = source.join("build");
    if in_tree.join("CMakeCache.txt").is_file() {
        return in_tree;
    }
    out_dir.join(shape.build_dir_name())
}

/// Two paths name one directory when both exist and resolve the same — `/tmp` behind a symlink is ordinary on
/// macOS, and a trailing separator is ordinary in an environment variable.
fn same_directory(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Configure unless the cache already says what the pin asks for. A cache that disagrees is
/// reconfigured rather than trusted: linking a tree built with different options (shared libs, or
/// curl on) would put bytes in `np` that nothing in this repository chose.
fn configure(
    cmake: &OsString,
    source: &Path,
    build_dir: &Path,
    out_dir: &Path,
    pin: &Pin,
    cross: Option<&Path>,
) -> Result<(), String> {
    let portable = portable_requested();
    let mut cache = read_cache(&build_dir.join("CMakeCache.txt"));

    // **A cmake cache remembers which source tree it was generated from, and refuses another.** Pointed at a
    // different `-S` it fails with "does not match the source … used to generate cache", so switching between
    // `.vendor/llama.cpp-<tag>` and `NP_LLAMA_CPP_SRC` — the two ways this build is legitimately reached, and the
    // difference between a CI cache and a developer's tree — would break the build with an error that names
    // neither cause nor cure. Wipe the directory cargo owns and configure fresh; never touch someone else's build
    // tree without saying so.
    if let Some(found) = cache.get("CMAKE_HOME_DIRECTORY") {
        let cached = Path::new(found);
        if !same_directory(cached, source) {
            if !build_dir.starts_with(out_dir) {
                return Err(format!(
                    "{} was configured for {} and is not inside cargo's OUT_DIR, so this build script \n  \
                     will not delete it. Remove that directory (it is generated), or point NP_LLAMA_CPP_SRC \n  \
                     back at {found}.",
                    build_dir.display(),
                    cached.display()
                ));
            }
            eprintln!(
                "np-core: {} holds a cache generated for {}; wiping it to configure {}",
                build_dir.display(),
                cached.display(),
                source.display()
            );
            fs::remove_dir_all(build_dir)
                .map_err(|e| format!("cannot wipe {}: {e}", build_dir.display()))?;
            cache.clear();
        }
    }
    let stale: Vec<String> = pin
        .options
        .iter()
        .filter(|(key, want)| {
            // cmake stores an `-DFOO=BAR` for a not-yet-declared option under the `UNINITIALIZED`
            // type, so only the value is comparable here, not the whole line.
            !cache
                .get(key.as_str())
                .is_some_and(|found| found.eq_ignore_ascii_case(want))
        })
        .map(|(key, want)| format!("{key}={want}"))
        .collect();

    // **A cache cannot tell you that an option stopped existing, and it cannot tell you that one was removed
    // from the pin.** cmake files an `-DFOO=` for an option no `CMakeLists.txt` declares under type
    // `UNINITIALIZED`: measured here, `LLAMA_CURL:UNINITIALIZED=OFF` while every live option is `BOOL`, and
    // v0.8.0 turns that flag into a deprecation notice (`CMakeLists.txt: llama_option_depr(WARNING LLAMA_CURL)`).
    // Since the reuse test compares values only — deliberately, so UNINITIALIZED entries look fresh — a typo or
    // an upstream rename reads as "matches the pin" forever while the build uses upstream's default, and deleting
    // a line from the pin leaves the old cached value in charge. Both faces are closed here: the exact `-D` set is
    // recorded next to the cache, and after configuring every pinned key must exist with a real type.
    let option_stamp = build_dir.join(OPTION_STAMP);
    let recorded = fs::read_to_string(&option_stamp).unwrap_or_default();
    // The stamp is the pin's option set **plus the digest of the `CMakeLists.txt` about to be configured** (A2 review
    // P1-4). This script declares that file a rerun input, so editing it re-runs build.rs — but with a stamp made only
    // of pinned options the warm path below finds every cached value still matching and returns without configuring,
    // certifying a build directory for inputs cmake never saw. Naming the file in `rerun-if-changed` is worth doing
    // only if something acts on it; this line is that something.
    // The cross arguments belong in the stamp even though they come from this invocation rather than the pin.
    // Left out — as they were on the first attempt — a warm **host** cache is certified to a windows build:
    // measured 2026-11-08, "already configured for the pin's options; skipping configure" followed by an archive
    // set built with the host compiler and no `lib` prefix. Same reasoning as the CMakeLists digest above: name
    // an input only if something acts on it.
    let mut extras = match cross {
        Some(toolchain) => format!(
            "toolchain={}\nGGML_NATIVE=OFF\nCMAKE_STATIC_LIBRARY_PREFIX=lib\n",
            toolchain.display()
        ),
        None => String::new(),
    };
    if portable {
        // Part of the configure's identity: a warm cache configured for this machine must not be certified fresh for
        // a portable one, or `NP_LLAMA_PORTABLE=1` would print its promise and reuse the AVX-512 archives anyway.
        extras.push_str("GGML_NATIVE=OFF\n");
        for option in PORTABLE_ISA_OPTIONS {
            extras.push_str(&format!("{option}=OFF\n"));
        }
        // The compiler flags belong to the configure's identity for the same reason: they, not the options above, are what
        // decided the instruction set on 2026-11-13, so a cache that predates them must not be certified fresh.
        extras.push_str(&format!(
            "CMAKE_C_FLAGS={}\nCMAKE_CXX_FLAGS={}\n",
            portable_cflags("CFLAGS"),
            portable_cflags("CXXFLAGS")
        ));
    }
    let wanted = format!(
        "{}{}cmakelists-sha256={}\n",
        pin.option_stamp(),
        extras,
        sha256_hex(
            &fs::read(source.join("CMakeLists.txt")).map_err(|e| format!(
                "cannot read {} to stamp the configure: {e}",
                source.join("CMakeLists.txt").display()
            ))?
        )
    );
    let set_changed = !recorded.is_empty() && recorded != wanted;
    // A cache with no stamp is not "unchanged": it was configured before this file recorded anything, so nothing here
    // can claim its values came from the pin. That is a change, and below it means a fresh directory.
    let untracked_cache = recorded.is_empty() && !cache.is_empty();

    if cache.contains_key("CMAKE_BUILD_TYPE")
        && stale.is_empty()
        && !set_changed
        && !untracked_cache
    {
        // Warm cache: still verify it understood our options. A cache that filed half of them as UNINITIALIZED
        // is exactly the state a reuse path must not certify as fresh.
        verify_options_understood(build_dir, pin)?;
        // And on a cross build, re-check the threading model: the stamp proves the same toolchain file was named,
        // not that its compilers still are win32 drivers — someone edits that file between builds.
        if cross.is_some() {
            require_win32_threading(build_dir)?;
        }
        eprintln!(
            "np-core: {} already configured for the pin's options; skipping configure",
            build_dir.display()
        );
        return Ok(());
    }

    if set_changed || untracked_cache {
        // **A reconfigure cannot undo a deletion.** cmake keeps a cache entry whose `-D` disappeared, so removing
        // `GGML_OPENMP = "OFF"` from the pin would leave `GGML_OPENMP:BOOL=OFF` in charge while that file's own preamble
        // promises unlisted options keep llama.cpp's default — and R6d's stated lever is flipping exactly that line, so
        // this failure mode sits one edit away (A2 review P1-2). Only a fresh directory fixes it. Ours to wipe when it
        // lives under `OUT_DIR`; a build directory inside someone's source checkout is not ours to delete, so that case
        // refuses and names the command — refuse rather than repair, as everywhere else in this file.
        let reason = if untracked_cache {
            "that build directory carries no option stamp".to_string()
        } else {
            format!(
                "the option set changed ({} -> {} stamped lines)",
                recorded.lines().count(),
                wanted.lines().count()
            )
        };
        if build_dir.starts_with(out_dir) {
            eprintln!(
                "np-core: {}, and cmake keeps cache entries whose -D disappeared; wiping {} and configuring fresh",
                reason,
                build_dir.display()
            );
            fs::remove_dir_all(build_dir)
                .map_err(|e| format!("cannot wipe {}: {e}", build_dir.display()))?;
        } else {
            return Err(format!(
                "np-core needs a fresh cmake configure because {}, but {} sits inside the source tree {} and is not \
                 ours to delete.\n  Remove it and rebuild:\n    rm -rf \"{}\"\n  A reconfigure in place would leave any \
                 option removed from {} still in force, which contradicts that file's promise that unlisted options keep \
                 llama.cpp's own default.",
                reason,
                build_dir.display(),
                source.display(),
                build_dir.display(),
                PIN
            ));
        }
    }
    if !build_dir.starts_with(out_dir) && !cache.is_empty() {
        // Reconfiguring a tree that is not ours to own is defensible — linking a tree built with shared libs or
        // extra targets on would put bytes in `np` that nothing here chose — but it must be said out loud, in
        // terms of whose directory it is.
        eprintln!(
            "np-core: reconfiguring {} (a build directory inside the source tree {}) to match {}'s options: {}",
            build_dir.display(),
            source.display(),
            PIN,
            if stale.is_empty() {
                "option set changed".to_string()
            } else {
                stale.join(", ")
            }
        );
    }

    fs::create_dir_all(build_dir)
        .map_err(|e| format!("cannot create {}: {e}", build_dir.display()))?;
    let mut args: Vec<String> = vec![
        "-S".to_string(),
        source.display().to_string(),
        "-B".to_string(),
        build_dir.display().to_string(),
    ];
    // **A release asset is run by machines that did not build it, and llama.cpp has no static runtime dispatch.**
    // Both halves matter, and the first draft of this fix got the second one wrong.
    //
    // `GGML_NATIVE=ON` (upstream's default for a native build) means `-march=native`: SIMD chosen by the CPU of
    // whoever ran cmake. Measured 2026-11-13 on the first CI-built Linux asset, built on a hosted runner: 14,540
    // `%zmm` instructions in `np`, and `np --version` on an x86-64 host whose flags stop at SSE4.2 answered
    // `Illegal instruction (core dumped)` before printing anything.
    //
    // Switching it off does **not** buy runtime dispatch on a static build — that is `GGML_CPU_ALL_VARIANTS`, which
    // upstream refuses unless `GGML_BACKEND_DL` is on (`ggml/src/CMakeLists.txt:491`), and dynamic backends are a
    // folder of `.so`s, not the single file this project ships. What `GGML_NATIVE=OFF` actually does on a native
    // build is flip `INS_ENB` to ON (`ggml/CMakeLists.txt:141`), which turns on `GGML_SSE42`, `GGML_AVX`,
    // `GGML_AVX2`, `GGML_FMA`, `GGML_F16C` and `GGML_BMI2` — and those become `-msse4.2 … -mavx2` flags on the whole
    // cpu backend (`ggml/src/ggml-cpu/CMakeLists.txt:350-371`). Measured on the next rehearsal asset: AVX-512 gone,
    // `%ymm` references **up** from 43,295 to 46,358, and `SIGILL` again at the same instruction (`c5 f9 ef c0`, a
    // VEX-encoded `paddq`, which needs AVX even in its 128-bit form). That draft made every build require an AVX2
    // machine, which is a wider floor than the one it replaced.
    //
    // So portable means the floor, named: native off **and** each instruction-set option off, which is exactly what
    // a cross build already gets because `GGML_NATIVE_DEFAULT` is OFF there and `INS_ENB` follows it. The Windows
    // asset has been baseline x86-64 since v0.2.2; this makes the Linux one say the same thing.
    //
    // **That explanation was complete for a developer's machine and wrong about the runner**, and the third, fourth and
    // fifth rehearsals proved it: with every option above OFF, `compile_commands.json` holding no `-m` token in any of 76
    // commands, and `RUSTFLAGS=-C target-cpu=x86-64` set, the asset still carried the same 46,358 wide-vector instructions
    // and still refused to start here. The census named the files (`ggml-quants.c`: 6,972; `arch/x86/repack.cpp`: 6,534;
    // `llamafile/sgemm.cpp`: 4,028 — all of them **zero** in a local build of the same pin), and one question to the
    // compiler named the cause: on that runner, `cc` with an empty command line predefined `__AVX__` and `__AVX2__`. Its
    // *default* target is above the floor, so nothing had to ask for AVX — the objects were born with it. Options can only
    // withdraw what something requested; [`PORTABLE_MARCH_FLAG`] states the floor to the compiler itself, and
    // [`require_portable_codegen`] asks the compiler whether that worked rather than trusting that it did.
    //
    // Deliberately opt-in rather than the default, because the cost lands on whoever builds for their own machine:
    // `cargo install` with `-march=native` is llama.cpp's fast path and the builder is the user there. The release
    // scripts set this variable; [`require_portable_codegen`] then proves from cmake's own cache that it took.
    if portable {
        args.push("-DGGML_NATIVE=OFF".to_string());
        for option in PORTABLE_ISA_OPTIONS {
            args.push(format!("-D{option}=OFF"));
        }
        // **…and name the floor to the compiler as well.** Those options only withdraw features something asked for, and
        // nothing here asked — the objects came out AVX2 anyway, because this runner's `cc` assumes AVX2 when told nothing.
        args.push(format!("-DCMAKE_C_FLAGS={}", portable_cflags("CFLAGS")));
        args.push(format!("-DCMAKE_CXX_FLAGS={}", portable_cflags("CXXFLAGS")));
    } else if cross.is_none() {
        // Say the quiet part where a developer will read it: this tree is being compiled for *this* machine.
        println!(
            "cargo::warning=np-core: llama.cpp is being built with `-march=native` (GGML_NATIVE defaults to ON). \
             Fine on this machine; set {PORTABLE_ENV}=1 for anything that will run elsewhere, see `np --version` \
             refusing to start on a pre-AVX2 host (docs/reference-releases.md)."
        );
    }
    if let Some(toolchain) = cross {
        args.push(format!("-DCMAKE_TOOLCHAIN_FILE={}", toolchain.display()));
        // Cross builds get the portable floor from upstream itself (`INS_ENB` follows `GGML_NATIVE_DEFAULT`, which
        // is OFF when the target is not the host), so only the native switch has to be made here.
        args.push("-DGGML_NATIVE=OFF".to_string());
        // mingw gives a static library **no prefix**: the build tree came out as `ggml.a`, `ggml-cpu.a`,
        // `ggml-base.a`, `libllama.a` (measured 2026-11-08), and GNU ld only ever searches `lib<name>.a` for
        // `-l <name>`. Asking cmake for the Unix prefix keeps one archive spelling across both targets, so
        // [`find_archive`]'s "this target stopped producing that library" check stays a real check instead of
        // firing on every Windows build — and rustc's `static=` flags resolve without link-arg gymnastics.
        args.push("-DCMAKE_STATIC_LIBRARY_PREFIX=lib".to_string());
    }
    for (key, value) in &pin.options {
        args.push(format!("-D{key}={value}"));
    }
    eprintln!(
        "np-core: configuring llama.cpp in {} ({} changed: {})",
        build_dir.display(),
        stale.len(),
        if stale.is_empty() {
            "first configure".to_string()
        } else {
            stale.join(", ")
        }
    );
    run(cmake, &args, "configure")?;
    verify_options_understood(build_dir, pin)?;
    require_portable_codegen(build_dir, portable)?;
    if cross.is_some() {
        require_win32_threading(build_dir)?;
    }

    fs::write(&option_stamp, wanted)
        .map_err(|e| format!("cannot write {}: {e}", option_stamp.display()))?;
    Ok(())
}

/// Every option in the pin must appear in the cache with a real type. `UNINITIALIZED` means cmake stored the
/// value because we asked it to and no `CMakeLists.txt` ever read it — so the build ran with upstream's default
/// while the pin claimed a setting. Measured on this tree: `LLAMA_CURL:UNINITIALIZED=OFF`, which v0.8.0 turns
/// into a deprecation notice, and that is how this check earned its place.
fn verify_options_understood(build_dir: &Path, pin: &Pin) -> Result<(), String> {
    let cache = build_dir.join("CMakeCache.txt");
    let types = read_cache_types(&cache);
    let ignored: Vec<String> = pin
        .options
        .iter()
        .filter(|(key, _)| {
            matches!(
                types.get(key.as_str()).map(String::as_str),
                None | Some("UNINITIALIZED")
            )
        })
        .map(|(key, value)| format!("{key}={value}"))
        .collect();
    if !ignored.is_empty() {
        return Err(format!(
            "cmake accepted these options from {} (or from this feature shape) but filed them as \n  \
             UNINITIALIZED, meaning this revision has no such option — the build ran with upstream's \n  \
             default instead:\n    {}\n  \
             Either the option was renamed upstream (find the new name and re-pin it) or it is misspelled. \n  \
             Leaving them in place would let {} keep claiming settings that nothing applied.",
            PIN,
            ignored.join(", "),
            PIN
        ));
    }
    Ok(())
}

/// Build the shape's targets with cmake.
///
/// `--target` is repeated per target rather than given a positional list: that is the spelling cmake documents for
/// several targets, and it keeps the text-only command line byte-identical to what it was before `vision` existed.
fn compile(cmake: &OsString, build_dir: &Path, targets: &[&str]) -> Result<(), String> {
    let mut args: Vec<String> = vec!["--build".to_string(), build_dir.display().to_string()];
    for target in targets {
        args.push("--target".to_string());
        args.push(target.to_string());
    }
    args.push("--parallel".to_string());
    args.push(jobs().to_string());
    run(cmake, &args, "build").map(drop)
}

/// `-j` for cmake: `NP_LLAMA_CPP_JOBS`, else cores capped at [`DEFAULT_JOB_CEILING`].
fn jobs() -> usize {
    // A set-but-unusable value says so on the record instead of being silently replaced by the default — the rule
    // `resolve_source` and `cmake_command` follow for their variables (A2 review P2-8).
    if let Some(raw) = env::var_os("NP_LLAMA_CPP_JOBS") {
        match raw.to_str().and_then(|s| s.trim().parse::<usize>().ok()) {
            Some(parsed) if parsed > 0 => return parsed,
            _ => println!(
                "cargo::warning=NP_LLAMA_CPP_JOBS={} is not a positive integer; using the default job count",
                raw.to_string_lossy()
            ),
        }
    }
    let cores = fs::metadata("/proc/cpuinfo")
        .and_then(|_| std::thread::available_parallelism().map(|n| n.get()))
        .unwrap_or(1);
    cores.min(DEFAULT_JOB_CEILING)
}

/// Run cmake, capturing both streams. Output is only interesting when something failed, which is
/// exactly when it becomes part of the panic message; on success a build script that echoed 40 lines
/// of configure chatter would be noise.
fn run(cmake: &OsString, args: &[String], step: &str) -> Result<String, String> {
    let output = Command::new(cmake)
        .args(args)
        .output()
        .map_err(|e| format!("cannot run {:?}: {e}", cmake))?;
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    if !output.status.success() {
        return Err(format!(
            "cmake {step} failed ({})\n  command: {cmake:?} {}\n{}",
            output.status,
            args.join(" "),
            indent(&tail(&text, 4000))
        ));
    }
    Ok(text)
}

/// The last `max` bytes — for a failing compile, the reason is at the end, and the full log of a
/// 40-line configure plus a 2-minute build would bury it.
fn tail(text: &str, max: usize) -> String {
    let bytes = text.as_bytes();
    if bytes.len() <= max {
        return text.to_string();
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("... (earlier output trimmed)\n{}", &text[start..])
}

fn indent(text: &str) -> String {
    text.lines().map(|line| format!("  | {line}\n")).collect()
}

/// Emit the link instructions: the shape's static archives from our build, then the libraries they need.
fn emit_link_flags(build_dir: &Path, shape: &Shape, windows: bool) -> Result<(), String> {
    let archives = shape.archives();
    let built = shape.targets().join(", ");
    let mut searched: Vec<PathBuf> = Vec::new();
    for name in &archives {
        let archive = find_archive(build_dir, name, &built)?;
        let dir = archive
            .parent()
            .ok_or_else(|| format!("{} has no parent directory", archive.display()))?;
        if !searched.contains(&dir.to_path_buf()) {
            searched.push(dir.to_path_buf());
            println!("cargo::rustc-link-search=native={}", dir.display());
        }
    }
    // The order above is **load-bearing**, and that is a measurement rather than a style rule. The spike's link wrapped all
    // six archives in `-Wl,--start-group … --end-group` (`vision-spike-2026-11-02.md`, "Link"), so its printed order carried
    // no information; here GNU ld resolves strictly left to right, and `libggml.a` ↔ `libggml-cpu.a` refer to each other.
    // A linker group was tried on 2026-11-03 and cannot be emitted this way: `-C link-arg` values land *after* rustc's own
    // `-l static=` flags in the `cc` line (verified with `cargo build -vv`), so `--start-group` would open a group behind
    // the libraries it is meant to protect. Order, therefore, is the mechanism — and a re-pin that moves a symbol between
    // two of these archives will fail with an undefined reference no local edit explains, which is what this comment is for.
    for name in &archives {
        println!("cargo::rustc-link-lib=static={name}");
    }

    let system = if windows {
        &WINDOWS_SYSTEM_LIBS[..]
    } else {
        &SYSTEM_LIBS[..]
    };
    for name in system {
        println!("cargo::rustc-link-lib=dylib={name}");
    }
    if windows {
        // The C++ runtime, and the *spelling* is a measurement: emitted as `dylib=stdc++` the linker produced an
        // exe importing **`libstdc++-6.dll`** — a mingw runtime file that is not on anyone's clean Windows
        // machine, found by reading the import table rather than by watching the link fail (measured 2026-11-08).
        // Named as a static archive it links `libstdc++.a` in and the import table drops to system DLLs. `gcc_eh`
        // joins it because C++ exceptions need the unwinder, and dynamic would break the same promise.
        println!("cargo::rustc-link-lib=static=stdc++");
        println!("cargo::rustc-link-lib=static=gcc_eh");
    }

    let cache = read_cache(&build_dir.join("CMakeCache.txt"));
    let openmp_on = cache
        .get(OPENMP_CACHE_KEY)
        .or_else(|| cache.get("GGML_OPENMP"))
        .is_some_and(|value| value.eq_ignore_ascii_case("ON") || value == "1");
    if openmp_on {
        println!("cargo::rustc-link-lib=dylib={OPENMP_LIB}");
    }
    eprintln!(
        "np-core: linked {archives:?} + system libs (OpenMP: {})",
        if openmp_on { "on" } else { "off" }
    );
    Ok(())
}

/// Find `lib<name>.a` (or `<name>.lib`) under the build directory. Recursive rather than hardcoded to
/// `src/` and `ggml/src/`, because those paths are this revision's layout, not a promise — but bounded
/// so a stray tree cannot make a build script wander forever.
///
/// `built` names the target set inside a refusal, so a missing `libmtmd.a` reads as "this shape asked for mtmd and the
/// build did not produce it" rather than as a search-path mystery.
fn find_archive(build_dir: &Path, name: &str, built: &str) -> Result<PathBuf, String> {
    // Three spellings, because two build systems and one upstream `set()` are involved: Unix cmake writes
    // `libggml.a`, MSVC would write `ggml.lib`, and mingw writes `ggml.a` — `ggml/CMakeLists.txt:80` clears the
    // static prefix for its own targets on Windows (measured 2026-11-08, while [`stage_prefixed_archives`] exists
    // to hand the linker a name it will actually open). A tree carrying two of these for one library still
    // refuses below rather than choosing by spelling.
    let names = [
        format!("lib{name}.a"),
        format!("{name}.lib"),
        format!("{name}.a"),
    ];
    let mut found: Vec<PathBuf> = Vec::new();
    let mut queue: Vec<PathBuf> = vec![build_dir.to_path_buf()];
    while let Some(dir) = queue.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let file_name = entry.file_name().to_string_lossy().into_owned();
            if path.is_dir() {
                // Build-system bookkeeping, never an archive we want.
                if file_name != "CMakeFiles" {
                    queue.push(path);
                }
            } else if names.contains(&file_name) {
                found.push(path);
            }
        }
    }

    found.sort();
    if found.len() > 1 {
        // A reconfigure that moved the output directory leaves two `libllama.a` under one build tree, and
        // choosing the lexicographically first would pick by spelling rather than by which build is current —
        // plausibly the stale one. Measured today: exactly one per name, so this fires only on the next revision.
        return Err(format!(
            "found {} archives named for `{name}` under {}; refusing to guess which build is current:\n    {}",
            found.len(),
            build_dir.display(),
            found
                .iter()
                .map(|p| format!("{}", p.display()))
                .collect::<Vec<_>>()
                .join("\n    ")
        ));
    }
    found.into_iter().next().ok_or_else(|| {
        format!(
            "the build produced no {name} archive under {}.\n  \
             It was configured with the options in {PIN} (plus this feature shape's own) and built with `--target` \
             for [{built}]; if that target set ever stops producing this library, the archive list in build.rs needs \
             to change deliberately — not a silent skip.",
            build_dir.display(),
        )
    })
}

/// Where the exact `-D` set passed to cmake is recorded, beside the cache it describes. Its absence means
/// "configured before this file existed"; a difference means an option was added, changed **or removed** — the
/// one edit a value comparison against the cache can never see, because cmake keeps the old value.
const OPTION_STAMP: &str = ".np-cmake-options";

/// The parsed subset of the pin we build from: the tag, and the `[cmake]` table in file order.
struct Pin {
    tag: String,
    /// sha256 of `include/llama.h` in the pinned tree — see the pin file's comment for why a tarball digest
    /// and a version triple are not enough to identify the header our FFI mirror copies.
    header_sha256: String,
    options: Vec<(String, String)>,
}

impl Pin {
    /// A line scan for `key = "value"`, with `[section]` headers — no TOML crate here on purpose
    /// (build-dependencies are dependencies, and §14's rule is that we add none to avoid bindgen).
    /// Anything that is not that shape is refused rather than ignored: a silently dropped option is a
    /// binary built with settings nobody wrote down.
    ///
    /// `extra` is the feature shape's own options ([`Shape::extra_options`], e.g. `LLAMA_BUILD_MTMD=ON`), appended
    /// after the pinned ones so that one list describes what cmake is about to be told — which is what
    /// [`Pin::option_stamp`] records and [`verify_options_understood`] checks, for both shapes.
    fn read(path: &Path, extra: &[(&str, &str)]) -> Result<Self, String> {
        let text = read(path)?;
        let mut tag = None;
        let mut header_sha256 = None;
        let mut options = Vec::new();
        let mut section = None;

        for (number, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(name) = line
                .strip_prefix('[')
                .and_then(|rest| rest.strip_suffix(']'))
            {
                section = Some(name.to_string());
                continue;
            }
            let (key, value) = line.split_once('=').ok_or_else(|| {
                format!(
                    "{}:{number}: not a `key = \"value\"` line: {line}",
                    path.display()
                )
            })?;
            let value = value.trim();
            let value = value
                .strip_prefix('"')
                .and_then(|rest| rest.strip_suffix('"'))
                .ok_or_else(|| {
                    format!(
                        "{}:{number}: {} must be a quoted string on one line: {line}",
                        path.display(),
                        key.trim()
                    )
                })?;
            match section.as_deref() {
                None if key.trim() == "tag" => tag = Some(value.to_string()),
                None if key.trim() == "header_sha256" => header_sha256 = Some(value.to_string()),
                // `url` and the tarball `sha256` are the vendor script's business; a build script that could
                // download them would be the thing §14 rules out. `header_sha256` is ours: it identifies what
                // is already on disk.
                None => {}
                Some("cmake") => options.push((key.trim().to_string(), value.to_string())),
                Some(other) => {
                    return Err(format!(
                        "{}:{number}: unknown section [{other}] in the pin file",
                        path.display()
                    ));
                }
            }
        }

        let tag = tag.ok_or_else(|| format!("{}: no `tag` in the pin file", path.display()))?;
        // Required rather than optional: a pin without it would silently restore the weaker check that R6b0's
        // review rejected — "some 0.8.x tree linked" reported as "the pinned tree answered".
        let header_sha256 = header_sha256.ok_or_else(|| {
            format!(
                "{}: no `header_sha256` in the pin file. It is the digest of the header \
                 `crates/np-core/src/llama_ffi.rs` mirrors; a re-pin updates it deliberately.",
                path.display()
            )
        })?;
        if options.is_empty() {
            return Err(format!(
                "{}: no `[cmake]` options, so the build would use llama.cpp's own defaults — which \
                 include building tests, tools, examples and the app (§14 chose otherwise on size grounds).",
                path.display()
            ));
        }
        // The shape's additions come last so the pinned file order stays what a reader of `third_party/llama-cpp.toml`
        // sees, and so turning a feature off never silently reorders an option that is still in force.
        options.extend(
            extra
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string())),
        );
        Ok(Self {
            tag,
            header_sha256,
            options,
        })
    }

    /// The option set as written to [`OPTION_STAMP`]: file order, one `key=value` per line. Deterministic
    /// because it is compared byte for byte against the previous configure's record.
    fn option_stamp(&self) -> String {
        self.options
            .iter()
            .map(|(key, value)| format!("{key}={value}\n"))
            .collect()
    }
}

/// `KEY:TYPE=VALUE` lines of a CMakeCache file; missing or unreadable is an empty map, which the
/// callers read as "not configured yet".
fn read_cache(path: &Path) -> std::collections::BTreeMap<String, String> {
    let mut entries = std::collections::BTreeMap::new();
    let Ok(text) = fs::read_to_string(path) else {
        return entries;
    };
    for line in text.lines() {
        if line.starts_with('#') || !line.contains(':') {
            continue;
        }
        if let Some((key, rest)) = line.split_once(':') {
            if let Some((_, value)) = rest.split_once('=') {
                entries.insert(key.to_string(), value.trim().to_string());
            }
        }
    }
    entries
}

/// `KEY:TYPE=VALUE` — the type half, which [`read_cache`] drops. It is the only evidence of whether cmake
/// recognised an option at all (`BOOL`/`STRING`/`FILEPATH` mean it exists; `UNINITIALIZED` means nobody read it).
fn read_cache_types(path: &Path) -> std::collections::BTreeMap<String, String> {
    let mut entries = std::collections::BTreeMap::new();
    let Ok(text) = fs::read_to_string(path) else {
        return entries;
    };
    for line in text.lines() {
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if let Some((key, rest)) = line.split_once(':') {
            if let Some((ty, _)) = rest.split_once('=') {
                entries.insert(key.to_string(), ty.to_string());
            }
        }
    }
    entries
}

fn read(path: &Path) -> Result<String, String> {
    fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))
}

/// Trim ASCII whitespace from both ends of an `OsStr` **without requiring it to be UTF-8** — the reason
/// `resolve_source` and `cmake_command` use `var_os` in the first place (A2 review P2-8). `OsStr::trim` is not stable
/// on this toolchain, and `to_string_lossy().trim()` would put replacement characters into a path that was fine.
fn os_trim(raw: &std::ffi::OsStr) -> OsString {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let bytes = raw.as_bytes();
        let ws = |b: &u8| b.is_ascii_whitespace();
        let first = bytes.iter().position(|b| !ws(b)).unwrap_or(bytes.len());
        let last = bytes
            .iter()
            .rposition(|b| !ws(b))
            .map(|i| i + 1)
            .unwrap_or(first);
        let range = first.min(last)..first.max(last);
        OsString::from(std::ffi::OsStr::from_bytes(&bytes[range]))
    }
    #[cfg(not(unix))]
    {
        // The build refuses non-Linux targets before either caller runs, so this arm exists to keep the file
        // compilable rather than to be right about exotic encodings.
        OsString::from(raw.to_string_lossy().trim())
    }
}

fn path_env(name: &str) -> Result<PathBuf, String> {
    env::var_os(name)
        .map(PathBuf::from)
        .ok_or_else(|| format!("{name} is not set"))
}

/// SHA-256 (FIPS 180-4) over a whole file, written out rather than pulled from a crate: this script's rule is
/// zero build-dependencies (§14 added none precisely so bindgen could not creep back in), and the alternative
/// to hashing is trusting a directory name — which is what this replaces. A wrong implementation fails loudly
/// as a refusal, never as a wrong binary.
///
/// Verified against coreutils `sha256sum` on 2026-10-29 for: an empty file, a 3-byte file, and files of 55,
/// 56 and 64 bytes (the three padding shapes), 3 MB of random bytes, and the pinned `include/llama.h` itself.
fn sha256_hex(bytes: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    // Padding: the message, one 1 bit, zeros to 56 mod 64, then the 64-bit big-endian bit length.
    let bit_len = (bytes.len() as u64).wrapping_mul(8);
    let mut block: Vec<u8> = Vec::with_capacity(bytes.len() + 72);
    block.extend_from_slice(bytes);
    block.push(0x80);
    while block.len() % 64 != 56 {
        block.push(0);
    }
    block.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in block.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[4 * i],
                chunk[4 * i + 1],
                chunk[4 * i + 2],
                chunk[4 * i + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let mut v = h;
        for i in 0..64 {
            let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
            let ch = (v[4] & v[5]) ^ ((!v[4]) & v[6]);
            let t1 = v[7]
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
            let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let t2 = s0.wrapping_add(maj);
            v[7] = v[6];
            v[6] = v[5];
            v[5] = v[4];
            v[4] = v[3].wrapping_add(t1);
            v[3] = v[2];
            v[2] = v[1];
            v[1] = v[0];
            v[0] = t1.wrapping_add(t2);
        }
        for i in 0..8 {
            h[i] = h[i].wrapping_add(v[i]);
        }
    }

    let mut out = String::with_capacity(64);
    for word in h {
        out.push_str(&format!("{word:08x}"));
    }
    out
}

/// The flags a portable tree hands cmake for C and C++: whatever the environment asked for, then the floor.
///
/// Ours goes last on purpose. gcc honours the rightmost `-march=`, so an inherited `CFLAGS=-march=native` cannot smuggle the
/// builder's CPU into a release asset. A developer who wants their own machine's tuning leaves `{PORTABLE_ENV}` unset, where
/// nothing here touches their flags at all.
fn portable_cflags(env_name: &str) -> String {
    let inherited = env::var(env_name).unwrap_or_default();
    format!("{} {PORTABLE_MARCH_FLAG}", inherited.trim())
        .trim()
        .to_string()
}

/// Whether this invocation asked for a portable tree, and what it says when the answer is neither yes nor no.
///
/// An unparseable value is reported rather than ignored quietly: `NP_LLAMA_PORTABLE=yes-please` falling back to
/// "not portable" would ship an asset tuned to the runner, which is the exact failure this variable exists to
/// prevent — so it prints a warning naming what it saw and what it did.
fn portable_requested() -> bool {
    let Some(raw) = env::var_os(PORTABLE_ENV) else {
        return false;
    };
    let text = raw.to_string_lossy().trim().to_ascii_lowercase();
    match text.as_str() {
        "" => false,
        "0" | "off" | "no" | "false" => false,
        "1" | "on" | "yes" | "true" => true,
        other => {
            println!(
                "cargo::warning={PORTABLE_ENV}={other} is not a yes/no value; building for this machine \
                 (`-march=native`) rather than portably. Set it to 1 for anything that will run elsewhere."
            );
            false
        }
    }
}

/// That a build asked to be portable actually configured itself that way.
///
/// The `-D` arguments above are claims; cmake's own cache is the record. This is the same move
/// [`require_win32_threading`] makes about threading, and it exists because the first attempt at this fix passed
/// `-DGGML_NATIVE=OFF` **and still produced an AVX-requiring binary** — on a native build that flag switches `INS_ENB`
/// on, which turns the per-ISA options on, which become `-mavx2` on the whole cpu backend. A check that only read
/// `GGML_NATIVE` back would have called that tree portable. So every option that can raise the floor is read.
///
/// Only enforced when `{PORTABLE_ENV}` was requested: a developer's own build is allowed to be tuned to their own
/// machine, and refusing that here would make the fast path impossible rather than optional.
fn require_portable_codegen(build_dir: &Path, portable: bool) -> Result<(), String> {
    if !portable {
        return Ok(());
    }
    let cache_path = build_dir.join("CMakeCache.txt");
    let cache = read_cache(&cache_path);
    let mut raised = Vec::new();
    for option in std::iter::once("GGML_NATIVE").chain(PORTABLE_ISA_OPTIONS) {
        match cache.get(option) {
            Some(value) if value.eq_ignore_ascii_case("OFF") => {}
            // An absent key is not an OFF: it means upstream renamed or dropped the option and this script can no
            // longer say what the tree was compiled for.
            Some(value) => raised.push(format!("{option}={value}")),
            None => raised.push(format!("{option}=absent")),
        }
    }
    // **Then ask the compiler what it would compile for, with those options gone.** The option check above reads cmake's
    // intentions; this one measures the toolchain's defaults, and on 2026-11-13 only this one was true: every `GGML_<ISA>`
    // option OFF, no `-m` token in any of the tree's 76 compile commands, and a hosted runner's `cc` that predefined
    // `__AVX__`/`__AVX2__` from an empty command line. The asset built from that "portable" configuration still refused to
    // start on a pre-AVX machine. Options can withdraw features something asked for; they cannot withdraw the compiler's own
    // default target, which is why `{PORTABLE_MARCH_FLAG}` goes on the command line and why its effect is measured here.
    let mut above = Vec::new();
    if let Some(compiler) = cache.get("CMAKE_C_COMPILER") {
        match predefined_macros(
            compiler,
            cache.get("CMAKE_C_FLAGS").map(String::as_str).unwrap_or(""),
        ) {
            Ok(macros) => {
                for forbidden in PORTABLE_FORBIDDEN_MACROS {
                    if macros.iter().any(|seen| seen == forbidden) {
                        above.push(forbidden);
                    }
                }
            }
            // Not being able to ask is not evidence of anything, and a portable build cannot rest on "probably fine".
            Err(why) => return Err(format!("{PORTABLE_ENV} is set, but {why}")),
        }
    } else {
        return Err(format!(
            "{PORTABLE_ENV} is set, but {} records no CMAKE_C_COMPILER, so nothing here can say what it compiles for.",
            cache_path.display()
        ));
    }

    if raised.is_empty() && above.is_empty() {
        return Ok(());
    }
    if !raised.is_empty() {
        return Err(format!(
            "{PORTABLE_ENV} is set, but {} does not describe a portable tree: {}.\n  \
         A tree configured this way is compiled for one class of CPU — measured 2026-11-13 as a release asset \n  \
         full of VEX-encoded instructions that answered `Illegal instruction` on an x86-64 host without AVX. The \n  \
         portable floor is GGML_NATIVE=OFF **and** every GGML_<ISA> option OFF: llama.cpp's runtime dispatch lives \n  \
         in dynamic backend variants (`GGML_CPU_ALL_VARIANTS`, which needs `GGML_BACKEND_DL`), not in a static build.",
            cache_path.display(),
            raised.join(", ")
        ));
    }
    Err(format!(
        "{PORTABLE_ENV} is set, but {}'s C compiler still defines {} when handed this build's CMAKE_C_FLAGS \
         ({:?}) — its own default target is above baseline x86-64 and `{}` did not outrank it.\n  A release asset is run by \
         machines that did not build it: measured 2026-11-13, an asset built with every cmake option reading OFF answered \
         `Illegal instruction`\n  on a host whose CPU flags stop at SSE4.2, because the compiler's default — not any option — \
         decided its instruction set.",
        cache_path.display(),
        above.join(", "),
        PORTABLE_MARCH_FLAG,
        cache.get("CMAKE_C_FLAGS").map(String::as_str).unwrap_or("<unset>"),
    ))
}

/// The macros `compiler` predefines for C with `flags`, as a list of names.
///
/// `cc -dM -E -x c -` over empty input is the cheapest question that answers "what will this compile for": it reports the
/// target the compiler settles on when told nothing, which no amount of reading option tables or `-D` lists reveals.
fn predefined_macros(compiler: &str, flags: &str) -> Result<Vec<String>, String> {
    let mut cmd = Command::new(compiler);
    cmd.args(flags.split_whitespace())
        .args(["-E", "-dM", "-x", "c", "-"]);
    let output = cmd
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("cannot run {compiler} to ask what it compiles for: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "{compiler} exited {} when asked what it compiles for",
            output.status
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        // `#define __AVX2__ 1` — the name is the **second** token. Taking the third was caught by mutation, not by the
        // portable build: a probe that collects the values instead of the names finds nothing forbidden in any compiler.
        .filter(|line| line.starts_with("#define "))
        .filter_map(|line| line.split_whitespace().nth(1))
        .map(str::to_string)
        .collect())
}

/// A posix-threaded mingw-w64 produces an exe that imports `libwinpthread-1.dll`, and that DLL is not on a
/// Windows machine. Measured 2026-11-08: `-Wl,-Bstatic -lwinpthread` keeps the import, and so does naming
/// `libwinpthread.a` by full path, because mingw's static archive carries a `/DEFAULTLIB` directive for its own
/// import library. The only recipe that produced a clean import table built with the `-win32` compiler drivers,
/// where ggml's `std::mutex` rides Win32 primitives and no pthread runtime exists at all.
///
/// So the cache is read back after configuring: if the compiler cmake actually used is not a win32-threading
/// driver, this refuses now rather than shipping an exe that dies at the loader on someone's laptop.
fn require_win32_threading(build_dir: &Path) -> Result<(), String> {
    let compiler = configured_compiler(build_dir);
    match compiler {
        Some(value) if value.contains("win32") => Ok(()),
        Some(value) => Err(format!(
            "the cross toolchain built these archives with {value}, which is not a win32-threading driver.\n  \
             Measured: an exe linked against them imports `libwinpthread-1.dll`, and no `-static` spelling of \n  \
             `-lwinpthread` removes it (mingw's static archive carries a /DEFAULTLIB for its own import library). \n  \
             Use the `…-gcc-win32` / `…-g++-win32` compilers."
        )),
        None => Err(format!(
            "no compiler is recorded under {}, so the threading model of these archives cannot be checked. \n  \
             A cross build that cannot prove which runtime it linked is not one to ship.",
            build_dir.display()
        )),
    }
}

/// Which C/C++ compiler cmake actually used, read from where it records that fact.
///
/// Not from `CMakeCache.txt`: a toolchain file that sets `CMAKE_C_COMPILER` with a plain `set()` — the usual
/// spelling — keeps it an internal variable, and the cache then holds only the derived `_AR`/`_RANLIB` entries
/// (measured 2026-11-08: the file has `CMAKE_C_COMPILER_AR` and no `CMAKE_C_COMPILER`). cmake writes the real
/// answer into `CMakeFiles/<version>/CMakeCXXCompiler.cmake`, so that is read first, with the cache as a
/// fallback for toolchain files that do use `set(... CACHE ...)`.
fn configured_compiler(build_dir: &Path) -> Option<String> {
    let files = std::fs::read_dir(build_dir.join("CMakeFiles")).ok()?;
    for entry in files.flatten() {
        for name in ["CMakeCXXCompiler.cmake", "CMakeCCompiler.cmake"] {
            let candidate = entry.path().join(name);
            let Ok(text) = fs::read_to_string(&candidate) else {
                continue;
            };
            for prefix in ["set(CMAKE_CXX_COMPILER \"", "set(CMAKE_C_COMPILER \""] {
                if let Some(rest) = text.lines().find(|l| l.starts_with(prefix)) {
                    if let Some(value) = rest[prefix.len()..].split('"').next() {
                        if !value.is_empty() {
                            return Some(value.to_string());
                        }
                    }
                }
            }
        }
    }
    let cache = read_cache(&build_dir.join("CMakeCache.txt"));
    ["CMAKE_CXX_COMPILER", "CMAKE_C_COMPILER"]
        .iter()
        .filter_map(|key| cache.get(*key))
        .find(|value| !value.is_empty())
        .cloned()
}

/// Copy the shape's archives into one directory under `OUT_DIR`, each named `lib<name>.a`.
///
/// See the call site: upstream gives ggml no prefix on Windows and GNU ld requires one. Staging them together
/// also means the link line's **order** stays a statement about our archive list rather than about cmake's
/// directory layout — which is what [`emit_link_flags`]'s comment insists on.
fn stage_prefixed_archives(
    build_dir: &Path,
    out_dir: &Path,
    shape: &Shape,
) -> Result<PathBuf, String> {
    let stage = out_dir.join(LINK_STAGE_DIR);
    // A stale stage would link an archive this build did not produce — the same failure the deleted-tree check
    // exists for, one directory over. Remove it before filling it.
    if stage.exists() {
        fs::remove_dir_all(&stage).map_err(|e| format!("cannot clear {}: {e}", stage.display()))?;
    }
    fs::create_dir_all(&stage).map_err(|e| format!("cannot create {}: {e}", stage.display()))?;
    let built = shape.targets().join(", ");
    for name in shape.archives() {
        let from = find_archive(build_dir, name, &built)?;
        let to = stage.join(format!("lib{name}.a"));
        fs::copy(&from, &to)
            .map_err(|e| format!("cannot copy {} to {}: {e}", from.display(), to.display()))?;
    }
    eprintln!(
        "np-core: staged {} archives as lib*.a in {} (mingw names them without the prefix)",
        shape.archives().len(),
        stage.display()
    );
    Ok(stage)
}

/// Ask a mingw compiler where its own static runtime archives live, and return the directories to search.
///
/// `-print-file-name` is the question gcc answers with a path (or with the bare name it was given, meaning "not
/// found" — which is why the result is checked rather than trusted). The two archives are the C++ standard
/// library and the exception unwinder: both must be linked **statically** into a shipped Windows binary, because
/// their dynamic forms arrive as `libstdc++-6.dll`, a file that is not on a clean machine.
fn windows_runtime_search_dirs(compiler: &Path) -> Result<Vec<PathBuf>, String> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    for wanted in ["libstdc++.a", "libgcc_eh.a"] {
        let output = std::process::Command::new(compiler)
            .arg(format!("-print-file-name={wanted}"))
            .output()
            .map_err(|e| format!("cannot run {} to locate {wanted}: {e}", compiler.display()))?;
        if !output.status.success() {
            return Err(format!(
                "{} exited {} while asked for {wanted}",
                compiler.display(),
                output.status
            ));
        }
        let printed = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let found = PathBuf::from(&printed);
        if !found.is_absolute() || !found.is_file() {
            return Err(format!(
                "{} reports no static archive for {wanted} (it answered {printed:?}).\n  \
                 A cross build that cannot link the C++ runtime statically would ship an exe needing a DLL nobody \n  \
                 installed, so nothing is linked here.",
                compiler.display()
            ));
        }
        let dir = found
            .parent()
            .ok_or_else(|| format!("{} has no parent directory", found.display()))?;
        if !dirs.iter().any(|seen: &PathBuf| seen == dir) {
            dirs.push(dir.to_path_buf());
        }
    }
    Ok(dirs)
}
