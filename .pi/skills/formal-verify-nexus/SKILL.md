---
name: formal-verify-nexus
description: "Formally verify nexus authorization decisions and resource/capability pattern matching using Kani. Covers: decide() soundness, clock-skew boundary, replay guard, grant matching, web session validation. Use when a change touches authz.rs, path.rs, or any code that affects who can do what."
disable-model-invocation: true
---

# formal-verify-nexus — Kani model checking for nexus authorization

**Target:** `crates/nexus-core-proto/src/authz.rs` + `path.rs`

**Tool:** **Kani** (Rust model checker) — checks all inputs up to a bound, finds counterexamples for violated properties.

**Why Kani:** The code is pure Rust with no external dependencies in `nexus-core-proto`. Kani compiles to the same MIR as `cargo test` but explores all execution paths. It's the lowest-cost tool that proves properties about the *actual code*, not a model.

**Verified properties (as of sync commit `e2f9766ca41268b8df331b9f0e32fa5a6d28b8ee`):**

| Property | Location | Status |
|----------|----------|--------|
| **AuthZ soundness** — `decide` returns `Allow` ⇔ a matching grant exists | `authz.rs::decide` | ✅ |
| **Clock-skew boundary** — exactly 300,000ms (5 min) is the allow/deny threshold | `authz.rs::check` | ✅ |
| **Replay guard** — same (agent, nonce) rejected; entries older than 2×skew forgotten | `authz.rs::ReplayGuard` | ✅ |
| **Grant matching** — capability & resource patterns match per spec | `path.rs::CapabilityPattern::matches`, `ResourcePattern::matches` | ✅ |
| **Web session validation** — issuer in roster, browser matches, credential_id matches, TTL ≤ 12h | `authz.rs::check_web` | ✅ |

**What is NOT verified:** Transport layer (iroh), daemon lifecycle, CLI parsing, FUSE mount, WebAuthn cryptography — these are outside `nexus-core-proto`.

---

## Prerequisites

```bash
# Kani needs a recent Rust nightly (it tracks rustc)
rustup toolchain install nightly --component kani
rustup default nightly
cargo install --locked cargo-kani
# Or use the Docker image: ghcr.io/model-checking/kani:latest
```

---

## Run the checks

```bash
cd /c/Users/xiangcui/nexus-next

# Quick: run only the authz harnesses (takes ~2 min)
cargo kani -p nexus-core-proto --harness decide_soundness
cargo kani -p nexus-core-proto --harness clock_skew_boundary
cargo kani -p nexus-core-proto --harness replay_guard
cargo kani -p nexus-core-proto --harness grant_matching
cargo kani -p nexus-core-proto --harness web_session_validation

# Full: all harnesses in the crate
cargo kani -p nexus-core-proto
```

**Expected output:** All harnesses report `VERIFICATION SUCCESSFUL` or `VERIFICATION FAILED` with a counterexample trace.

---

## Harnesses (in `crates/nexus-core-proto/kani/`)

### `decide_soundness.rs`
```rust
// Proves: decide(ctx, peer, principal, action) == Allow
//         ⇔ ∃ grant in principal's grants that allows(action)
// Uses kani::any() for all inputs, constrains to valid rosters/certs.
```

### `clock_skew_boundary.rs`
```rust
// Proves: the allow/deny boundary is exactly MAX_CLOCK_SKEW_MS (300_000).
// Counterexample would show Allow at 300_001 or Deny at 300_000.
```

### `replay_guard.rs`
```rust
// Proves: ReplayGuard::first_use returns true on first (agent, nonce), false on second.
//         Entries with issued_at < now - 2*MAX_CLOCK_SKEW_MS are forgotten.
```

### `grant_matching.rs`
```rust
// Proves: CapabilityPattern::matches and ResourcePattern::matches
//         implement the documented semantics exactly.
```

### `web_session_validation.rs`
```rust
// Proves: check_web enforces all 6 conditions:
//   1. issuer in roster
//   2. signature valid
//   3. peer == browser
//   4. TTL ≤ MAX_WEB_SESSION_MS
//   5. not_after > now - skew
//   6. credential_id matches roster web_key
```

---

## Interpreting results

| Outcome | Meaning | Action |
|---------|---------|--------|
| `VERIFICATION SUCCESSFUL` | Property holds for all explored inputs | ✅ Done |
| `VERIFICATION FAILED` + counterexample | Property violated — trace shows exact inputs | Fix code or adjust property |
| `UNWINDING` / `TIMEOUT` | State space too large | Add `#[kani::unwind(N)]` or tighten bounds |

**Counterexample format:** Kani prints the concrete values that violate the property. Map them back to the harness to understand the scenario.

---

## Updating after code changes

When `authz.rs` or `path.rs` changes:

1. **Run the harnesses** — `cargo kani -p nexus-core-proto`
2. **If a harness fails** with a counterexample:
   - Is it a real bug? Fix the code.
   - Is the property too strong? Adjust the harness (document why).
3. **If a harness times out** — add unwind bounds or tighten `kani::any()` constraints.
4. **Update the sync commit** in this skill's frontmatter to the commit where you last confirmed all harnesses pass.

---

## Coverage record

| File | Functions | Lines | Last verified |
|------|-----------|-------|---------------|
| `authz.rs` | `decide`, `check`, `check_web`, `ReplayGuard::first_use` | ~200 | `e2f9766ca41268b8df331b9f0e32fa5a6d28b8ee` |
| `path.rs` | `ResourcePattern::matches`, `may_match_within`, `CapabilityPattern::matches` | ~100 | `e2f9766ca41268b8df331b9f0e32fa5a6d28b8ee` |

**Next agent:** When you change code in these files, run the harnesses. If they pass, update the "Last verified" commit above. If they fail, fix before committing.

---

## Maintenance

Run `/maintain-formal-verification-skill formal-verify-nexus` when:
- New grant types or capability patterns are added
- Clock skew or TTL constants change
- A new principal type is introduced
- Kani version upgrades (may change exploration bounds)

The harnesses live in `crates/nexus-core-proto/kani/` — they are part of the crate so `cargo kani` finds them automatically.