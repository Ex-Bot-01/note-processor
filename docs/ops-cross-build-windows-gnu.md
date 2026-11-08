# Cross-building a Windows `np.exe` from Linux — no Windows machine, no CI minutes (`F39`)

**Why this file exists.** While hosted Actions minutes are exhausted, README's cross-platform instruction is
`cargo install --git …`, and **F39** found a real user whose Windows refuses to *run* anything cargo just built
(`Eine Anwendungssteuerungsrichtlinie hat diese Datei blockiert`, os error 4551). For that machine — and for anyone on Smart App
Control, WDAC or an aggressive AV tier — "build it yourself" is not an instruction. The only door left is a **prebuilt binary**, and
producing one used to require either CI minutes or a Windows box. Neither is true: this repository's default-feature binary builds here,
on Linux, in about a minute.

## What was measured (2026-11-08, this host)

| thing | value |
| --- | --- |
| target triple | `x86_64-pc-windows-gnu` |
| command wall time | **46.71 s** (`cargo build --release --locked -p np-cli --target x86_64-pc-windows-gnu`) |
| `np.exe` | 14,001,466 B · sha256 `d36f3e3b7c0cebccee46911782a62b362a78979b942996ed4eb37f108444c270` |
| asset (exe + `SHA256SUMS.txt`) | 5,078,108 B · sha256 `3312c16a1d68f5ee6f925c7f19591581b546cf305cd95746b97441aef8583305` |
| imported DLLs (`objdump -p`) | `kernel32`, `ntdll`, `msvcrt`, `advapi32`, `bcrypt`, `bcryptprimitives`, `ws2_32`, `userenv`, `api-ms-win-core-synch-l1-2-0` — **all Windows system DLLs** |

That last row is the one that decides whether the artifact is usable: no `libgcc_s_seh-1.dll`, no `libwinpthread-1.dll`. The Rust
`windows-gnu` link already keeps the C runtime static, so nothing has to be installed beside `np.exe`. Had those two appeared, the
asset would have been a folder of DLLs and this document would say "not portable".

**Scripted 2026-11-09: `.github/scripts/cross-build-windows.sh`.** It does the steps below, refuses if the import table grows a mingw
runtime DLL (the check that decides whether `np.exe` is still one file), and prints both hashes because release notes quote both. Running it on
the v0.2.2 tree reproduced the published asset byte for byte — zip `9850dc0d…720ad6`, inner `np.exe` `d7104b59…4eef75` — which is the closest
thing here to a determinism claim: same toolchain, same tree, same bytes. The refusal path was exercised with a stubbed `objdump` that claims
`libwinpthread-1.dll`, and it stopped before packaging. This document stays as the *why*; the script is the *how*.

## The toolchain, without root and without apt-installing anything

`sudo` is not available on this host and mingw-w64 was not installed. It does not need either: Ubuntu's `.deb`s are `ar`+`tar`
archives, and `dpkg-deb -x` unpacks them anywhere.

```bash
mkdir -p /home/agent/tmp/mingwdebs && cd /home/agent/tmp/mingwdebs
apt-get install --print-uris -y gcc-mingw-w64-x86-64 | grep "^'" | sed "s/'\([^']*\)'.*/\1/" > uris.txt   # 10 URIs
cat uris.txt | xargs -n1 -P4 curl -sS -O
mkdir -p ../mingw && for d in *.deb; do dpkg-deb -x "$d" ../mingw; done

# Ubuntu ships the driver as `…-gcc-13-posix`/`…-gcc-posix` behind update-alternatives, which only runs on install.
cd ../mingw/usr/bin && ln -sf x86_64-w64-mingw32-gcc-13-posix x86_64-w64-mingw32-gcc
```

`ar`, `as`, `nm`, `ranlib`, `strip`, `windres`, `dlltool` already exist under their plain names; only `gcc` needed the symlink. Then:

```bash
cd /home/agent/note-processor
export PATH="/home/agent/tmp/mingw/usr/bin:$HOME/.cargo/bin:$PATH" TMPDIR=/home/agent/tmp
export CC_x86_64_pc_windows_gnu=x86_64-w64-mingw32-gcc
export AR_x86_64_pc_windows_gnu=x86_64-w64-mingw32-ar
export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc
rustup target add x86_64-pc-windows-gnu
cargo build --release --locked -p np-cli --target x86_64-pc-windows-gnu
```

Nothing in the repository changes: no `.cargo/config.toml`, no CI, no dependency. `ring` and the bundled SQLite inside
`libsqlite3-sys` are C, and both compile with this compiler — which is the whole reason a `-gnu` target works where a hand-rolled
MSVC-ish cross would not.

## What this artifact is, and what it is not

**It is** a real Windows x64 binary that exercises the same `#[cfg(windows)]` code paths CI cannot reach: `%LOCALAPPDATA%` placement
(**F37**), long-path prefixes, path keys, the identity fold behind rename tracking (**F18**), FTS5 and every search mode (**F36**).
Filesystem semantics — case-insensitivity, alternate data streams, `\\?\` behaviour — come from Windows, not from the C ABI, so a GNU
build answers those questions honestly.

**It is not** the MSVC build CI would ship, and three specific things follow:

* **No `local-model`.** Stage 6 (fetch → `embed` → inference) needs llama.cpp built for that target; CMake + mingw for a 2 M-line C/C++
  tree is its own project, and MSVC is what the pinned build instructions assume. So this asset tests search, config strictness, scan,
  identity and F37 — not inference.
* **It is unsigned.** Which matters more than it sounds: if F39's machine blocks *every* unknown executable rather than only
  compiler-spawned children, a downloaded `.exe` will not run either, and the remaining answers are code-signing or a policy exclusion —
  both of which change something in Windows. The 10-second test for that is whether an `np.exe` built *before* the policy tightened still
  runs on the same box.
* **It is cross-compiled, not CI-graded.** It carries no gate result. Say so wherever it is offered: this is a way to get evidence from a
  machine we cannot otherwise reach, not a release-quality artifact. The Linux asset (`v0.2.0`, built by `package-assets.sh`) and this one
  are different things and should not share a filename shape — hence the `-cross` suffix.

## Where this leaves distribution

| route | needs | buys |
| --- | --- | --- |
| **this cross-build** | nothing beyond this host | a Windows binary for F18/F36/F37 evidence, unsigned, no inference |
| make the repo public | one web-UI click (and a history review) | free hosted runners again → real MSVC assets **and** CI grading of `main`, which is what F21/F28/F36 closure paragraphs are waiting on |
| buy Actions minutes or Pro | money | the same as above, keeping the repo private |
| self-hosted runner | an Actions-scoped token (403 today) | full matrix off your own hardware |
| code signing | a certificate purchase | a Windows binary that survives Smart App Control / ASR — needed only if the test above shows downloads are blocked too |

## 2026-11-08: the asset now carries the inference runtime (this supersedes every "built without `local-model`" line above)

Everything above this line describes the runtime-less build, which is what v0.2.1 through v0.2.3 shipped and why `np embed` on a downloaded
asset answered `runtime not built`. That is no longer the shape we ship: `.github/scripts/cross-build-windows.sh` now builds
`--features local-model,fetch`, so llama.cpp v0.8.0 is compiled and statically linked into `np.exe`. The model is still never in the asset —
309 MB arrives only when a human runs `np setup` or `np embed`, digest-verified.

The recipe, and the four facts that took measurements to find:

| step | what it is | why it is not the obvious thing |
| --- | --- | --- |
| toolchain | mingw-w64 **win32-threading** drivers (`…-gcc-win32`, `…-g++-win32`), unpacked rootlessly from Ubuntu .debs | posix-threaded drivers produce an exe importing `libwinpthread-1.dll`, and no `-static` spelling of `-lwinpthread` removes it — mingw's static archive carries a `/DEFAULTLIB` directive for its own import library. With win32 drivers ggml's `std::mutex` rides Win32 primitives and the dependency does not exist |
| C++ compiler | `g++-mingw-w64-x86-64-win32`, same `.deb` trick as gcc | the gcc-only set that sufficed for a Rust-only exe cannot compile llama.cpp at all |
| cmake | the pin's own options **plus** `-DGGML_NATIVE=OFF` | `GGML_NATIVE=ON` means `-march=native`, which measures the machine running cmake: a Linux CPU deciding a Windows binary's SIMD paths |
| archives | staged as `lib<name>.a` copies in one directory of ours | `ggml/CMakeLists.txt:80` clears the static-library prefix on Windows, and GNU ld refuses to open `ggml.a` when asked for `-l ggml` ("rename it to libggml-base.a") |
| C++ runtime | `rustc-link-lib=static=stdc++` + `static=gcc_eh`, search dir asked out of the compiler with `-print-file-name` | as a dylib it arrives as an exe importing `libstdc++-6.dll`. rustc also searches only the `-L` paths we give it, so "could not find native static library `stdc++`" is answered by asking the compiler where its own archives live, not by hard-coding a path |

Import table of the shipped exe, which is the test that decides whether this is one file or a folder of DLLs:

```text
ADVAPI32.dll  api-ms-win-core-synch-l1-2-0.dll  bcrypt.dll  bcryptprimitives.dll
kernel32.dll  msvcrt.dll  ntdll.dll  USERENV.dll  WS2_32.dll
```

Two things that bit during packaging and are now written into the script:

* **`objdump -t` is not a test of a stripped binary.** It reported 2 symbols before `strip` and 0 after, so a symbol-table check refuses every
  properly packaged artifact. The guard reads strings instead; a runtime-less exe (v0.2.3's asset, used as the mutant) matches zero times.
* **`grep -q` under `set -o pipefail` fails a passing check.** `-q` exits at the first match, the producer dies on SIGPIPE, and `pipefail`
  reports 141 — the guard refused a binary containing the symbol four times before this was read rather than assumed.

Sizes, both printed by the script because the notes quote the archive and a report quotes the exe: **23,297,605 B → 16,253,440 B stripped**,
inside §14's ≤ 40 MB ceiling with room to spare. Stripping is new: every asset until today shipped unstripped while the ceiling has always
been quoted against stripped figures (v0.2.3's exe was 14,073,097 B where a stripped one is 9,952,768 B).

**What this does not claim.** The exe has never run on Windows — there is no Wine here, so `llama_model_load_from_file` has been reached on
Linux only. F18's rule still applies to the claim "semantic search works on Windows", and the first honest form of it comes from an operator
run. MSVC remains unbuilt and `build.rs` still refuses it, along with macOS.
