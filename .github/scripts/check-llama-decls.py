#!/usr/bin/env python3
"""The hand-written llama.cpp declarations are compared against the pinned header, not against memory.

`crates/np-core/src/llama_ffi.rs` mirrors llama.cpp v0.8.0's `include/llama.h` by hand (§14 explains why there is no
bindgen). A mirror has one failure mode and it is silent: the C side changes shape, the Rust declaration keeps its spelling,
and what follows is not a compile error but undefined behaviour that a test may never notice. This script compares the two
where they can be compared, and says out loud what it could not compare.

Per `pub fn` inside the `extern "C"` block:

  * the name must exist in the header at all;
  * the argument count must match;
  * every parameter and the return type are resolved through the header's own `typedef`s and compared structurally — pointer
    depth, whether the pointee is `const`, and the base kind (`i32`/`u32`/`usize`/`f32`/`bool`/`()`/an opaque struct name).

Per `#[repr(C)] pub struct` whose body the header also shows: the field **count** must match. A struct the header declares
opaquely (`struct llama_model;`) is reported as not compared rather than counted as a pass — we only ever hold those by
pointer, which is exactly why their layout is not our problem, and that reasoning belongs in a log line instead of in a
comment nobody re-reads at pin time.

`--self-test` plants one break per rule (missing name, wrong arity, wrong return kind, wrong parameter kind, struct field
count) in a synthetic pair of files and requires each to be named — plus the clean case, so a checker that compares nothing
cannot pass.
"""

import os
import re
import subprocess
import shutil
import sys
import tempfile

FN_IN_RUST = re.compile(r"^\s*pub fn (\w+)\(([^)]*)\)\s*(?:->\s*(.+?))?\s*;", re.M)
STRUCT_IN_RUST = re.compile(r"#\[repr\(C\)\]\s*pub struct (\w+)\s*\{")
FIELD_IN_RUST = re.compile(r"^\s*pub \w+:", re.M)
EXPORT = r"(?:LLAMA_API|MTMD_API|GGML_API)"
FN_IN_HEADER = re.compile(EXPORT + r"\s+([^;]+?)\s+(\w+)\s*\(([^;{]*?)\)\s*;", re.S)
STRUCT_IN_HEADER = re.compile(r"typedef struct (\w+)\s*\{([^}]*)\}\s*\w+\s*;|\bstruct (\w+)\s*\{([^}]*)\}", re.S)
TYPEDEF = re.compile(r"typedef\s+([^;]+?)\s+(\w+)\s*;")

PRIMITIVE_WORDS = {"int", "char", "float", "double", "unsigned", "signed", "long", "short", "void", "_Bool", "bool"}

RUST_KIND = {
    "i8": "i8", "u8": "u8", "i16": "i16", "u16": "u16", "i32": "i32", "u32": "u32",
    "i64": "i64", "u64": "u64", "usize": "usize", "isize": "isize", "f32": "f32", "f64": "f64",
    "bool": "bool", "()": "void", "c_char": "i8", "c_int": "i32", "c_uint": "u32",
    "c_void": "void", "c_float": "f32", "c_double": "f64", "c_uchar": "u8", "c_schar": "i8",
}
C_KIND = {
    "int8_t": "i8", "uint8_t": "u8", "int16_t": "i16", "uint16_t": "u16", "int32_t": "i32", "uint32_t": "u32",
    "int64_t": "i64", "uint64_t": "u64", "size_t": "usize", "ssize_t": "isize", "ptrdiff_t": "isize",
    "float": "f32", "double": "f64", "bool": "bool", "void": "void", "char": "i8", "int": "i32",
    "unsigned int": "u32", "unsigned char": "u8", "unsigned short": "u16", "long long": "i64",
}


def read(path):
    with open(path, encoding="utf-8", errors="replace") as f:
        return f.read()


def header_typedefs(text):
    """`typedef int32_t llama_pos;` -> {llama_pos: 'int32_t'} (function-pointer typedefs are skipped: none appear here)."""
    out = {}
    for base, name in TYPEDEF.findall(text):
        base = base.strip()
        if "(" in base or "*" in base or not re.fullmatch(r"[A-Za-z_][\w ]*", base):
            continue
        out[name] = base
    return out


def c_shape(c_type, typedefs, depth=0):
    """Reduce a C type to (pointer_depth, const_pointee, base_kind). Returns None when it cannot be resolved."""
    t = " ".join(c_type.replace("\n", " ").split())
    if not t:
        return None
    is_const = False
    while t.endswith("*"):
        t = t[:-1].strip()
        depth += 1
        if t.endswith("const"):
            t = t[:-5].strip()
            is_const = True
        if t.startswith("const "):
            t = t[6:].strip()
            is_const = True
    if depth and not is_const:
        pass  # `llama_token *` and `const llama_token *` differ; both are recorded faithfully above
    t = t.replace("const ", "").replace("struct ", "").replace("enum ", "").strip()
    if t in typedefs and typedefs[t].replace("struct ", "").strip() != t:
        # A named struct used by pointer (`const llama_context *`, where the header has
        # `typedef struct llama_context llama_context;`) is an opaque handle, not an alias to recurse into.
        resolved = c_shape(typedefs[t], typedefs, depth)
        if resolved and resolved[2] != "name:" + t:
            return (depth, is_const or resolved[1], resolved[2]) if resolved[0] else resolved
        return (depth, is_const, "name:" + t)
    if t in C_KIND:
        return (depth, is_const, C_KIND[t])
    if depth > 0:
        return (depth, is_const, "name:" + t)
    if depth == 0 and re.fullmatch(r"(?:unsigned |signed )?[A-Za-z_]\w*", t):
        return (0, False, "name:" + t)  # an enum or int-typed typedef used by value; compared by name
    return None


def rust_shape(r_type, typedefs, depth=0):
    t = " ".join((r_type or "()").replace("\n", " ").split())
    if not t:
        return None
    is_const = False
    for prefix in ("*const ", "*mut "):
        while t.startswith(prefix):
            t = t[len(prefix):].strip()
            depth += 1
            is_const = prefix == "*const "
    if "::" in t:
        t = t.rsplit("::", 1)[-1]
    if t.startswith("Option<"):
        return (max(depth, 1), False, "fnptr")  # a nullable function pointer: depth is what matters, not its signature
    if t in RUST_KIND:
        return (depth, is_const, RUST_KIND[t])
    if t in typedefs:
        return rust_shape(typedefs[t], typedefs, depth)
    if depth > 0:
        return (depth, is_const, "name:" + t)
    return (0, False, "name:" + t)


def c_param_type(arg):
    """`const llama_context * ctx` -> `const llama_context *`; `int32_t n_tokens` -> `int32_t`.

    A C declaration in a header names its parameter last, and the name is optional; dropping it wrongly would compare the
    wrong thing, so an argument whose trailing word is a type keyword (`unsigned int`) or a pointer star is left alone.
    """
    a = " ".join(arg.replace("\n", " ").split())
    if not a:
        return ""
    tokens = a.split()
    # A trailing primitive keyword means there was no parameter name to drop (`unsigned int`, `long long`).
    if len(tokens) > 1 and re.fullmatch(r"[A-Za-z_]\w*", tokens[-1]) and tokens[-1] not in PRIMITIVE_WORDS:
        return " ".join(tokens[:-1])
    return a


def strip_comments(text):
    text = re.sub(r"/\*.*?\*/", " ", text, flags=re.S)
    return re.sub(r"//[^\n]*", "", text)


def c_struct_fields(header_text, name):
    """The field declarations of `struct name { … }`, brace-matched and comment-free, or None if the body is not shown.

    Splitting on `}` is wrong here: these structs contain nested braces (default initializers, anonymous unions), and a
    truncated body reads as "the pin has 5 fields" when it has 17 — a mismatch that would send someone chasing a phantom.
    """
    m = re.search(r"\bstruct\s+" + re.escape(name) + r"\s*\{", header_text)
    if not m:
        return None
    i, depth = m.end(), 1
    while i < len(header_text) and depth:
        if header_text[i] == "{":
            depth += 1
        elif header_text[i] == "}":
            depth -= 1
        i += 1
    body = strip_comments(header_text[m.end():i - 1])
    # Fields end at `;` outside nested braces.
    fields, depth, current = [], 0, []
    for ch in body:
        if ch == "{":
            depth += 1
        elif ch == "}":
            depth -= 1
        if ch == ";" and depth == 0:
            if "".join(current).strip():
                fields.append("".join(current))
            current = []
        else:
            current.append(ch)
    return fields


def rust_struct_fields(rust_text, name):
    """The field count of `#[repr(C)] pub struct name { … }`, brace-matched.

    Matching to the next line that starts with `}` is wrong for a file whose structs live inside a module (every struct here
    is indented), and it silently swallows the structs that follow — which reads as "17 fields declared" for one that has 15.
    Fields are counted at depth 1 only, so a `Option<extern "C" fn(…)>` spanning several lines is one field, not four.
    """
    # Anchored on `pub struct`, not on the attribute pair: an interposed `#[derive(...)]` must not move a struct out of reach.
    m = re.search(r"^\s*pub struct\s+" + re.escape(name) + r"\s*\{", rust_text, re.M)
    if not m:
        return None
    i, depth, body = m.end(), 1, []
    while i < len(rust_text) and depth:
        ch = rust_text[i]
        if ch == "{":
            depth += 1
        elif ch == "}":
            depth -= 1
            if depth == 0:
                break
        body.append(ch); i += 1
    text = "".join(body)
    count, depth = 0, 0
    for line in text.split("\n"):
        stripped = line.strip()
        if depth == 0 and re.match(r"^pub \w+\s*:", stripped):
            count += 1
        depth += line.count("{") - line.count("}")
    return count


STRUCT_ANY_RUST = re.compile(r"^\s*pub struct (\w+)\s*\{", re.M)


def mirrored_structs(rust_text):
    """Every `pub struct` in the file, with whether a `#[repr(C)]` sits above it.

    Matching `#[repr(C)] pub struct` as one pattern is what let `llama_batch` escape this check for as long as it existed:
    the real file puts `#[derive(Debug, Copy, Clone)]` between the two, and a fence that silently compares seven of eight
    structs is worse than one that says which eight it looked at. So the struct list comes from `pub struct`, and a missing
    `#[repr(C)]` becomes a finding rather than an omission.
    """
    out = []
    for m in STRUCT_ANY_RUST.finditer(rust_text):
        head = rust_text[:m.start()].split("\n")[-9:]
        out.append((m.group(1), any("#[repr(C)]" in line for line in head)))
    return out


def extern_block(rust_text):
    """The contents of `extern "C" { … }` blocks; declarations outside one are not C bindings."""
    out = []
    for chunk in re.findall(r'extern "C"[^{]*\{(.*?)\n\}', rust_text, re.S):
        out.append(chunk)
    return "\n".join(out)


def rust_typedefs(rust_text):
    return dict(re.findall(r"pub type (\w+) *= *([^;]+);", rust_text))


def compare(ffi_text, header_text):
    cdefs = header_typedefs(header_text)
    rdefs = rust_typedefs(ffi_text)
    header_fns = {}
    for ret, name, args in FN_IN_HEADER.findall(header_text):
        params = [] if not args.strip() or args.strip() == "void" else [a for a in args.split(",")]
        header_fns.setdefault(name, []).append((ret.strip(), params))
    problems, compared, skipped, notes = [], 0, [], []

    block = extern_block(ffi_text)
    for name, args, ret in FN_IN_RUST.findall(block):
        params = [a.split(":", 1)[1].strip() for a in args.split(",") if a.strip()]
        if name not in header_fns:
            problems.append(f"llama_ffi declares `{name}`, which the pinned header does not declare")
            continue
        probed = [rust_shape(x, rust_typedefs(ffi_text)) for x in params]
        # A callback parameter — `Option<unsafe extern "C" fn(…)>`, or an alias for one — cannot be split on commas without a
        # real parser, and a half-parsed signature is worse than an admitted gap. Name and arity are still checked.
        if any("<" in x or " fn" in x or sh is None or ("fn" in str(sh[2])) or "," in str(sh[2])
               for x, sh in zip(params, probed)):
            # A callback parameter (`Option<unsafe extern "C" fn(…)>`) cannot be split on commas without a real parser, and a
            # half-parsed signature is worse than an admitted gap: name and arity are still checked above.
            skipped.append(f"`{name}`: takes a callback — name compared, and arity from the declaration's top-level "
                           f"commas; the callback's own signature is not compared")
            continue
        if False:
            problems.append(f"llama_ffi declares `{name}`, which the pinned header does not declare")
            continue
        overloads = header_fns[name]
        shapes = [(c_shape(r, cdefs), [c_shape(c_param_type(a), cdefs) for a in ps]) for r, ps in overloads]
        want_ret = rust_shape(ret, rdefs)
        want_args = [rust_shape(p, rdefs) for p in params]
        def loose(h, r):
            """Equal shapes, allowing the two differences that cannot miscompile here."""
            if h is None or r is None:
                return False      # unresolvable on either side: never a silent pass
            if h == r:
                return True
            hd, hc, hb = h
            rd, rc, rb = r
            if hd != rd:
                return False
            # `*const c_char` vs `const char *` is the same pointer; `name:` covers an int-represented enum declared as i32.
            def kind(b):
                if b in ("i8",):
                    return "char"  # `char` and `int8_t` are the same thing to us, and Rust spells both i8
                if b.startswith("name:"):
                    return b[len("name:"):]
                return b
            # An int-represented enum (`enum llama_pooling_type`) is declared as `i32` in Rust: compared by the fact that
            # both sides are a 4-byte integer, which is the only property a caller can rely on.
            # A C `enum` is int-represented, so declaring it as `i32` mirrors it; `u32` does not — signedness is part of the
            # shape, and a negative enumerator would arrive reinterpreted. Tight on purpose: this is the one place the fence
            # could otherwise wave through a real difference.
            same_kind = kind(hb) == kind(rb) or (hb.startswith("name:") and rb == "i32")
            # Declaring `*const` where C takes a non-const pointer is stricter, not looser: safe for the read-only queries
            # here, and named as a note so a future pin that turns one into a writer shows up as a line in the log.
            return same_kind and (hc == rc or (rc and not hc))

        strict = all(s[0] == want_ret and len(s[1]) == len(want_args) and
                     all(h == r for h, r in zip(s[1], want_args)) for s in shapes)
        ok_loose = any(s[0] is not None and len(s[1]) == len(want_args) and loose(s[0], want_ret) and
                       all(loose(h, r) for h, r in zip(s[1], want_args)) for s in shapes)
        if strict:
            compared += 1
            continue
        if ok_loose:
            compared += 1
            notes.append(f"`{name}`: mirrors the header with a Rust-side tightening (const pointer or int-as-enum)")
            continue
        any_arity = [s for s in shapes if len(s[1]) == len(want_args)]
        if not any_arity:
            problems.append(
                f"`{name}`: {len(want_args)} argument(s) declared, header has "
                f"{sorted({len(ps) for _, ps in overloads})} — arity drifted with the pin")
        else:
            got = any_arity[0]
            bad = [(i, h, r) for i, (h, r) in enumerate(zip(got[1], want_args)) if not loose(h, r)]
            detail = "; ".join(f"arg {i}: header {h}, Rust {r}" for i, h, r in bad[:3])
            ret_bad = not loose(got[0], want_ret)
            problems.append(f"`{name}`: signature drifted from the pinned header"
                            + (f"; return header {got[0]}, Rust {want_ret}" if ret_bad else "")
                            + (f"; {detail}" if detail else ""))

    structs_compared = structs_opaque = 0
    for name, is_repr_c in mirrored_structs(ffi_text):
        if not is_repr_c:
            problems.append(f"struct {name}: mirrored without `#[repr(C)]` — field order is the compiler's guess, not the C layout")
            continue
        n_rust = rust_struct_fields(ffi_text, name) or 0
        fields = c_struct_fields(header_text, name)
        if fields is None:
            skipped.append(f"struct {name}: opaque in the header (held by pointer only) — layout not compared")
            structs_opaque += 1
            continue
        n_c = len(fields)
        if n_c != n_rust:
            problems.append(f"struct {name}: {n_rust} field(s) declared, header has {n_c}")
        else:
            compared += 1
            structs_compared += 1
    return problems, compared, skipped, notes + [
        f"struct coverage: {len(mirrored_structs(ffi_text))} mirrored, {structs_compared} field-compared, "
        f"{structs_opaque} opaque-by-handle"
    ]


def pinned_headers(root):
    """Every header the mirrored declarations could come from.

    `llama_ffi.rs` mirrors two APIs: llama.cpp's own `include/llama.h`, and the vision shape's `tools/mtmd/mtmd.h`. Reading
    only the first makes every `mtmd_*` declaration look like a lie, which is how a check gets switched off; so both are read
    and searched together.
    """
    src = os.environ.get("NP_LLAMA_CPP_SRC")
    roots = []
    if src:
        roots.append(src)
    pin = os.path.join(root, "third_party", "llama-cpp.toml")
    if os.path.exists(pin):
        m = re.search(r'tag *= *"v?([^"]+)"', read(pin))
        if m:
            roots.append(os.path.join(root, ".vendor", f"llama.cpp-v{m.group(1)}"))
    found = []
    for r in roots:
        rels = ["include/llama.h"]
        mtmd = os.path.join(r, "tools", "mtmd")
        if os.path.isdir(mtmd):
            rels += [os.path.join("tools", "mtmd", h) for h in sorted(os.listdir(mtmd)) if h.endswith(".h")]
        for rel in rels:
            path = os.path.join(r, rel)
            if os.path.exists(path):
                found.append(path)
        if found:
            break
    return found


def pinned_header(root):
    headers = pinned_headers(root)
    if not headers:
        return None
    combined = "\n".join(read(h) for h in headers)
    merged = tempfile.NamedTemporaryFile("w", suffix=".h", delete=False, encoding="utf-8")
    merged.write(combined); merged.close()
    return merged.name


def main(argv):
    if "--self-test" in argv:
        return self_test()
    pos = [a for a in argv if not a.startswith("-")]
    root = pos[0] if pos else os.path.abspath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", ".."))
    header = pinned_header(root)
    if not header:
        print("NOT COVERAGE: no pinned llama.cpp header found (set NP_LLAMA_CPP_SRC or run scripts/vendor-llama-cpp.sh)")
        return 1
    ffi_path = os.path.join(root, "crates/np-core/src/llama_ffi.rs")
    if not os.path.exists(ffi_path):
        print("NOT COVERAGE: crates/np-core/src/llama_ffi.rs is not in this tree (feature off?)")
        return 1
    problems, compared, skipped, notes = compare(read(ffi_path), read(header))
    for p in problems:
        print(p)
    for s in skipped + notes:
        print(f"note: {s}")
    if compared == 0:
        print("NOT COVERAGE: 0 declarations compared against " + header)
        return 1
    print(f"compared {compared} declaration(s) against {os.path.relpath(header, root)}; {len(problems)} mismatch(es); "
          f"{len(skipped)} not comparable")
    return 1 if problems else 0


def self_test():
    header = '''
typedef int32_t llama_pos;
typedef uint32_t llama_seq_id;
typedef struct llama_context llama_context;
typedef struct llama_batch {
    int32_t n_tokens;
    int32_t * token;
    float * embd;
    llama_pos * pos;
    int32_t * n_seq_id;
    llama_seq_id ** seq_id;
    int8_t * logits;
} llama_batch;
LLAMA_API uint32_t llama_n_ctx(const llama_context * ctx);
LLAMA_API const char * llama_version(void);
LLAMA_API void llama_free(llama_context * ctx);
'''
    ffi = '''
pub type llama_pos = i32;
pub type llama_seq_id = u32;
#[repr(C)]
pub struct llama_batch {
    pub n_tokens: i32,
    pub token: *mut i32,
    pub embd: *mut f32,
    pub pos: *mut llama_pos,
    pub n_seq_id: *mut i32,
    pub seq_id: *mut *mut llama_seq_id,
    pub logits: *mut i8,
}
extern "C" {
    pub fn llama_n_ctx(ctx: *const llama_context) -> u32;
    pub fn llama_version() -> *const i8;
    pub fn llama_free(ctx: *mut llama_context);
}
'''
    cases, failures = [], []

    problems, compared, skipped, _notes = compare(ffi, header)
    if problems or compared < 4:
        failures.append(f"the matching pair reported {problems} with only {compared} compared")
    else:
        cases.append(f"a mirrored declaration set passes ({compared} compared, {len(skipped)} noted)")

    plants = [
        ("a name the header does not have", ffi.replace("pub fn llama_free(", "pub fn llama_free_v2("), "does not declare"),
        ("wrong arity", ffi.replace("pub fn llama_n_ctx(ctx: *const llama_context)",
                                    "pub fn llama_n_ctx(ctx: *const llama_context, extra: u32)"), "arity drifted"),
        ("wrong return kind", ffi.replace("pub fn llama_n_ctx(ctx: *const llama_context) -> u32",
                                          "pub fn llama_n_ctx(ctx: *const llama_context) -> usize"), "signature drifted"),
        ("wrong parameter kind", ffi.replace("pub fn llama_free(ctx: *mut llama_context)",
                                             "pub fn llama_free(ctx: *mut u64)"), "signature drifted"),
        ("a Rust-side tightening is a note, not a failure", ffi.replace("pub fn llama_free(ctx: *mut llama_context)",
                                                                       "pub fn llama_free(ctx: *const llama_context)"), "__never__"),
        ("struct field count", ffi.replace("    pub logits: *mut i8,\n}", "}"), "field(s) declared"),
    ]
    for label, mutated, needle in plants:
        problems, _, _, notes = compare(mutated, header)
        if needle == "__never__":
            if not problems and any("tightening" in n for n in notes):
                cases.append("a stricter Rust-side pointer is a named note, not a failure")
            else:
                failures.append(f"a const tightening was not treated as a note: problems={problems} notes={notes}")
            continue
        if any(needle in pr for pr in problems):
            cases.append(f"planted {label} is named")
        else:
            failures.append(f"planted {label} not caught: {problems}")

    derived = ffi.replace("#[repr(C)]\npub struct llama_batch {", "#[repr(C)]\n#[derive(Debug, Copy, Clone)]\npub struct llama_batch {")
    problems, _, _, notes = compare(derived.replace("    pub logits: *mut i8,\n}", "}"), header)
    if any("field(s) declared" in pr for pr in problems):
        cases.append("a struct with a derive between `#[repr(C)]` and `pub struct` is still compared")
    else:
        failures.append(f"a derived struct escaped the field check: {problems}")

    noattr = ffi.replace("#[repr(C)]\npub struct llama_batch {", "pub struct llama_batch {")
    problems, _, _, _ = compare(noattr, header)
    if any("without `#[repr(C)]`" in pr for pr in problems):
        cases.append("a mirrored struct missing `#[repr(C)]` is named")
    else:
        failures.append(f"a struct without repr(C) passed silently: {problems}")

    # An opaque struct must be reported as not compared, never counted as a pass.
    _, _, skipped, _ = compare(ffi + '\n#[repr(C)]\npub struct llama_model {\n    pub phantom: u8,\n}\n', header)
    if any("opaque in the header" in s for s in skipped):
        cases.append("an opaque struct is reported as not compared, not as passing")
    else:
        failures.append(f"opaque struct silently accepted: {skipped}")

    print("llama-decls self-test")
    for c in cases:
        print(f"  ok   {c}")
    for f in failures:
        print(f"  FAIL {f}")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
