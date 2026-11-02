#!/usr/bin/env python3
"""Compare the vectors a thread sweep dumped, one row per pair against the baseline.

Usage: thread_vectors_compare.py <baseline.f32> <other.f32> [more.f32 ...]

Each file is one raw little-endian float-32 row written by `crates/np-core/tests/local_threads.rs` (768 coordinates,
3072 bytes). The name carries the thread count — `vec-8.f32` — because a table that says "vector A" and "vector B"
cannot be read back against a log row.

Two numbers per pair, deliberately:
  * cosine — the search-relevant question. §14's noise floor for "the same text, re-prefixed" sat at 0.8956, so a
    cross-thread-count cosine near 1.0 means ranking is unaffected;
  * max |Δ| — because cosine can read 0.99999 while one coordinate moves by an amount that matters when a caller
    compares stored bytes. A digest over the vector's bytes (which is what `embeddings` stores) changes on a single
    flipped bit, so this number decides whether re-embedding at a different thread count would rewrite rows.

Exit code is 0 always; the table is the product. A missing/short file is an error, because a truncated dump compared as
if it were complete would say "identical" about nothing.
"""
import array
import os
import sys


def load(path):
    size = os.path.getsize(path)
    with open(path, "rb") as handle:
        raw = handle.read()
    if len(raw) != size or size % 4:
        raise SystemExit(f"{path}: short or misaligned read ({len(raw)} of {size} bytes)")
    vec = array.array("f")
    vec.frombytes(raw)
    return vec


def main():
    if len(sys.argv) < 3:
        raise SystemExit(__doc__)
    base_path = sys.argv[1]
    base = load(base_path)
    print(f"BASELINE   {os.path.basename(base_path)} dim={len(base)}")
    print("PAIR       cosine            max_abs_diff    verdict")
    for path in sys.argv[2:]:
        other = load(path)
        if len(other) != len(base):
            raise SystemExit(f"{path}: dim {len(other)} != baseline {len(base)}")
        dot = norm_a = norm_b = 0.0
        max_abs = 0.0
        for x, y in zip(base, other):
            dot += x * y
            norm_a += x * x
            norm_b += y * y
            diff = abs(x - y)
            if diff > max_abs:
                max_abs = diff
        cosine = dot / ((norm_a ** 0.5) * (norm_b ** 0.5)) if norm_a and norm_b else 0.0
        # Byte-identity is the property `embeddings` depends on for "same bytes, same row", so it is stated as its own
        # verdict rather than inferred from a small max_abs_diff.
        identical = all(x == y for x, y in zip(base, other))
        verdict = "bit-identical" if identical else f"differs ({max_abs:.3e} worst coordinate)"
        print(f"PAIR       {os.path.basename(base_path)} vs {os.path.basename(path)}: cosine={cosine:.12f} max|d|={max_abs:.6e} -> {verdict}")


if __name__ == "__main__":
    main()
