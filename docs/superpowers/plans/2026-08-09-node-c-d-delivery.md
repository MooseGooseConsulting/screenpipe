# Node C and D Delivery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` task-by-task. Steps use checkbox syntax for tracking.

**Goal:** Deliver consent-safe independent screen, browser, clipboard, and audio observations, then turn them into replayable context, gap, boundary, and checkpoint facts.

**Architecture:** Node B at Screenpipe `bf307fa10` is the reviewed base. Source adapters emit one typed, policy-epoch-bound observation contract without writing policy rows directly. Node D owns the durable domain mapping, boundary/gap/checkpoint persistence, and PostgreSQL transactions; source adapters only call those APIs after their own pre-access and pre-attachment checks.

**Tech Stack:** Rust, SQLx/PostgreSQL 18, Windows capture/UIA/WASAPI, Tokio, GitHub Actions, Pester.

## Global Constraints

- `docs/build/DAG.md` and `docs/system/contracts.md` in Pieces are authoritative; capability-map state changes require exact Screenpipe and Pieces commits plus sanitized evidence.
- Keep Screenpipe's existing event IDs and compatibility readers usable; PostgreSQL is canonical and raw images/audio are never durable.
- Canonical `window_key` is exactly `none` or `window(hwnd=<unsigned integer>,window_generation=<unsigned integer>)`; no zero/fake HWND or modality-free epoch.
- Every observation has source, modality, machine, timestamp, producer/version, policy epoch, optional window, typed reason, and content-free timing metadata.
- Clipboard and audio begin ungranted. Exclusion wins. Policy checks occur before payload/device access and before durable attachment; revoked work must close with a restart-stable policy-close key.
- Preserve source-lane isolation: C1 owns screen files, C2 browser files, C3 clipboard files, C4 audio files. D alone owns `screenpipe-memory` domain/persistence and final shared composition changes.
- Follow TDD: every behavior test must be observed failing for the missing behavior before production code is written; run only remote GitHub PostgreSQL services, never local containers.

---

### Task 1: Node D typed observation and window identity foundation

**Files:**
- Create: `crates/screenpipe-memory/src/observation.rs`
- Modify: `crates/screenpipe-memory/src/lib.rs`, `crates/screenpipe-memory/src/sample.rs`, `crates/screenpipe-memory/src/runner.rs`, `crates/screenpipe-cli/src/main.rs`
- Test: `crates/screenpipe-memory/tests/observation_outcomes.rs`

**Interfaces:**
- Produces `ObservationOutcome::{Available,Absent,Denied,Failed,TimedOut,Cancelled,Stale}`, `ObservationIdentity`, and `WindowKey::None|Window { hwnd, window_generation }`.
- `ObservationIdentity::validate()` rejects empty source/modality/producer/version, a missing epoch, and an absent window encoded as a fake value.
- The shared CLI composition root renders `RunOutcome::Outcome` as a fixed category/status line and never logs outcome content or timing values. Existing adapters use an explicit `LegacySample` compatibility variant until their C1/C3/C4 migrations; only the typed `Sample` variant may reach the policy-bound runner path and it requires a validated `Available` outcome.

- [ ] Write a failing test that parses `none` and `window(hwnd=42,window_generation=7)`, rejects `window(hwnd=0,window_generation=7)`, and rejects an `Available` outcome lacking producer/version.
- [ ] Run `cargo test -p screenpipe-memory --test observation_outcomes -- --test-threads=1`; confirm the target fails because the module/API is absent.
- [ ] Implement the public types, canonical formatter/parser, validation, and library exports with no persistence side effects.
- [ ] Re-run the target, `cargo test -p screenpipe-memory --lib`, and `cargo check -p screenpipe-cli`; confirm all pass.
- [ ] Commit `feat(memory): add typed observation outcomes`.

### Task 2: C1 policy-first screen request and stale deduplication

**Files:**
- Modify: `crates/screenpipe-cli/src/windows_source.rs`, `crates/screenpipe-cli/src/main.rs`
- Test: `crates/screenpipe-cli/src/windows_source.rs` tests and `crates/screenpipe-cli/tests/screen_policy.rs`

**Interfaces:**
- Consumes `ObservationIdentity`, the policy repository's committed snapshot, and `WindowKey`.
- Produces a request record before capture and `Denied`/`Stale` without retaining pixels when policy, HWND/generation, or epoch changed.

- [ ] Write a failing scripted-ops test proving excluded screen policy invokes neither WGC capture nor fingerprint; write a second test proving an epoch change after capture yields `Stale` and does not update the dedup cache.
- [ ] Run the named tests; confirm each fails for the missing request/policy behavior.
- [ ] Implement pre-acquisition policy lookup, post-acquisition/pre-fingerprint and pre-commit revalidation, and dedup keyed by source/modality/window/epoch.
- [ ] Re-run the named tests plus `cargo test -p screenpipe-cli --test screen_policy` and the owning CLI test target.
- [ ] Commit `feat(screen): enforce policy-first stale-safe capture`.

### Task 3: C2 independent browser observation and non-blocking correlation

**Files:**
- Create: `crates/screenpipe-cli/src/browser_provider.rs`, `crates/screenpipe-cli/tests/browser_provider.rs`
- Modify: `crates/screenpipe-cli/src/main.rs`

**Interfaces:**
- `BrowserRequestKey` is exactly `(source_id, hwnd, window_generation, browser_policy_epoch, observation_timestamp)`.
- Provider completion returns a typed observation independently; correlation receives an immutable completion and cannot delay screen capture.

- [ ] Write failing tests for a pending browser request cancelled on epoch change, a late completion rejected before attachment, and a screen request completing while browser work is blocked.
- [ ] Run `cargo test -p screenpipe-cli --test browser_provider -- --test-threads=1`; confirm RED for missing provider behavior.
- [ ] Implement the provider, cancellation token, independent persistence handoff, and optional correlation path.
- [ ] Re-run the target and `cargo test -p screenpipe-screen`; confirm browser failure does not stop screen capture.
- [ ] Commit `feat(browser): persist independent policy-bound observations`.

### Task 4: C3 default-off clipboard revocation

**Files:**
- Modify: `crates/screenpipe-cli/src/clipboard_source.rs`, `crates/screenpipe-cli/src/main.rs`
- Test: `crates/screenpipe-cli/tests/clipboard_policy.rs`

**Interfaces:**
- Clipboard source consumes a committed `(source_id, clipboard)` policy snapshot before opening the buffer.
- A policy-close transition stops pending reads and emits only typed/content-free outcome telemetry.

- [ ] Write failing tests proving ungranted policy never opens the clipboard, revocation during a pending read yields `Cancelled` without a text event, and diagnostics contain neither text nor its hash/length.
- [ ] Run `cargo test -p screenpipe-cli --test clipboard_policy -- --test-threads=1`; confirm RED.
- [ ] Implement policy-gated source startup, cancellation, and content-free close handling.
- [ ] Re-run the target plus the existing clipboard source tests.
- [ ] Commit `feat(clipboard): enforce default-off revocable reads`.

### Task 5: C4 policy-gated microphone and system audio

**Files:**
- Modify: `crates/screenpipe-cli/src/audio_source.rs`, `crates/screenpipe-audio/src/lib.rs`, `.github/workflows/ci.yml`
- Test: audio-source tests and `cargo test -p screenpipe-cli --features audio` in the pinned Windows job

**Interfaces:**
- Audio channel maps microphone and system audio to distinct source identities and `WindowKey::None`.
- Device opening consumes the committed audio policy; revocation terminates active work without durable raw audio.

- [ ] Write failing scripted-audio tests proving ungranted policy never opens a device and microphone/system samples carry distinct source identity with no HWND.
- [ ] Run the focused audio tests; confirm RED because policy gating is absent.
- [ ] Implement pre-device gate, policy-close cancellation, transcript bounds, and raw-audio-negative diagnostics.
- [ ] Run the focused test, `cargo test -p screenpipe-audio`, and require the pinned Windows audio workflow to pass.
- [ ] Commit `feat(audio): gate distinct channels by durable policy`.

### Task 6: Node D boundaries, gaps, checkpoints, and PostgreSQL durability

**Files:**
- Create: `crates/screenpipe-memory/src/gaps.rs`, `crates/screenpipe-memory/src/checkpoint.rs`
- Modify: `crates/screenpipe-memory/src/context.rs`, `crates/screenpipe-memory/src/postgres.rs`, `crates/screenpipe-memory/src/runner.rs`, `crates/screenpipe-memory/src/sample.rs`, `crates/screenpipe-memory/src/lib.rs`
- Test: `crates/screenpipe-memory/tests/gap_ledger.rs`, `crates/screenpipe-memory/tests/checkpoints.rs`, `crates/screenpipe-memory/tests/context_graph.rs`

**Interfaces:**
- `GapFact` reuses its durable observation timestamp on retry; policy-close uses `(source_id, modality, committed_policy_epoch, close_kind)`.
- Boundary records carry a version and one reason: content hash, source/window generation, app/title, idle/session, policy, or explicit close.

- [ ] Write failing PostgreSQL tests for duplicate replay not creating a second gap, recovery producing one stable close, a late fact preserving ordered projection, and a crash losing no more than the configured checkpoint interval.
- [ ] Run each named integration target with `doppler run -p homelab -c dev_personal -- cargo test -p screenpipe-memory --test <target> -- --test-threads=1`; confirm RED reflects missing behavior rather than missing credentials.
- [ ] Implement append-only insert-on-conflict behavior, replay-safe projections, persisted active-boundary state, direct-outage status, and checkpoint recovery.
- [ ] Remove the `LegacySample` compatibility path after C1, C3, and C4 emit validated `Available` observations; add a regression test proving no content-bearing observation bypasses typed identity/outcome validation.
- [ ] Re-run all three integration targets plus `cargo test -p screenpipe-memory`.
- [ ] Commit `feat(memory): persist replay-safe boundaries gaps and checkpoints`.

### Task 7: Cross-repository evidence and promotion gate

**Files:**
- Modify: `C:/_projects/pieces_memory_observations/docs/system/capability-map.md`
- Create: `C:/_projects/pieces_memory_observations/verify/tests/node-c-d-integrity.Tests.ps1`

**Interfaces:**
- Evidence identifies exact Screenpipe/Pieces commits, commands, result, environment, limitations, and content-free artifacts.

- [ ] Write a failing Pester assertion that rejects a C/D promotion without both exact commits, target results, and evidence tier.
- [ ] Run the Pester target; confirm RED for absent promotion validator.
- [ ] Implement the validation and update only capability rows proven by the attached exact evidence.
- [ ] Run deterministic Pieces validation and the approved Doppler-backed Pester suites.
- [ ] Commit the Pieces evidence change and open a ready-for-review PR linked to the Screenpipe PR.
