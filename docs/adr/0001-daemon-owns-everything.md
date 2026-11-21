# ADR 0001 — The daemon owns everything; the window is a dumb client

**Status:** accepted. **Asked:** How the pieces are arranged, asked before any of them existed.
**Recorded here:** 2026-11-20, moved verbatim from `todo.md`'s "Locked decisions" list; a ruling date quoted below is as it
was recorded there.

## Decision

Shape: **daemon owns everything, Tauri window is a dumb client** (`np-core` / `npd` / `np-app`). Login
autostart so today's file and TODO segment exist before the app opens. Linux **and** Windows.
