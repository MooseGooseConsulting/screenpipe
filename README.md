# MooseGoose Screen Memory

This repository is the headless Windows vision component for Goal 1 of the
MooseGoose screen-memory system. It is an independent MIT repository derived
from Screenpipe commit `892199f742e46d0c5d9e8c06687b35ca7c2b6547`.

The current development branch is not a completed runtime release. Capture,
OCR, deterministic merging, adaptive cadence, PostgreSQL writing, the `run`
loop, `doctor`, and per-user service lifecycle boundaries are implemented and
tested. Service installation and controlled live capture proof remain separate
gates before the runtime may be considered operational.

## Product boundary

The workspace contains only:

- `screenpipe-screen`: foreground Windows capture, transient frames, OCR, and
  metadata-only Chrome/Edge address-bar access;
- `screenpipe-memory`: deterministic OCR identity, merge decisions, cadence,
  and capture-to-sink orchestration;
- `screenpipe-cli`: the minimal `run`, `doctor`, and `service` command surface.

There is no desktop UI, cloud sync, marketplace, updater, SQLite runtime,
summarization, embedding, MCP server, or model-based merge decision.

## Build and offline verification

```powershell
cargo fmt --all -- --check
cargo test --workspace
cargo check --workspace
powershell -NoProfile -File .\scripts\verify-pruned.ps1
```

The three interactive Windows tests remain ignored by default. They require an
unlocked desktop with a controlled foreground window and must not be treated as
live proof merely because the normal workspace tests pass.

## Runtime and secrets contract

PostgreSQL is the only canonical store. The runtime reads the connection string
from `SCREEN_MEMORY_DATABASE_URL`, injected by Doppler without putting its value
in arguments, logs, Git, or this document:

```powershell
doppler run -p homelab -c dev_personal -- .\target\release\screenpipe.exe doctor
doppler run -p homelab -c dev_personal -- .\target\release\screenpipe.exe run --machine-slug icarus --display-name Icarus-Laptop
```

The checked-in `doppler.yaml` contains only the safe accessible workstation
project/config names `homelab/dev_personal`. `doctor` verifies the variable is
present, PostgreSQL is version 18 or newer, the authoritative tables and
machine identity are ready, and the interactive Windows/OCR prerequisites are
available without rendering the connection value.

## Search and local output privacy

Search reads only an existing machine identity; it never creates or updates a
machine. Supply ranked words, a time window, or both:

```powershell
doppler run -p homelab -c dev_personal -- .\target\release\screenpipe.exe search --machine-slug icarus --since yesterday --limit 10 release notes
```

Search prints local terminal results. Those results can include matched OCR
snippets and stored browser URLs, so treat the terminal as sensitive output and
do not redirect it to files, logs, shared consoles, or other private-output
destinations unless that disclosure is intended.

## Per-user service contract

The service lifecycle owns exactly these current-user artifacts:

- task `MooseGoose Screen Memory`;
- `%LOCALAPPDATA%\screen-memory\bin\screenpipe.exe`;
- `%LOCALAPPDATA%\screen-memory\run-screenpipe.ps1`.

The task uses the current user's interactive token at limited privilege and
starts at that user's logon. The wrapper invokes the existing Doppler
`homelab/dev_personal` namespace and never contains the database URL. Uninstall
removes only the exact task, copied binary, and wrapper; it preserves the
PostgreSQL cluster, logs, and all other files beneath the screen-memory root.

Do not install the service until `screenpipe doctor` passes against native
PostgreSQL 18+, the authoritative schema, OCR, and controlled foreground WGC.

## Privacy boundary

Frames exist only as transient in-memory BGRA buffers and are zeroed on drop.
They are never written to disk or PostgreSQL. OCR supplies content text. UI
Automation is restricted to app/window metadata and the Chrome or Edge address
bar. Logs and debug output do not include OCR bodies, readable text, URL query
strings, secret values, frame bytes, or frame fingerprints.
