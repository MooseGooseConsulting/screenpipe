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

- `screenpipe-screen`: foreground Windows capture, transient frames, OCR,
  metadata-only Chrome/Edge address-bar access, and the clipboard text
  boundary;
- `screenpipe-memory`: deterministic OCR identity, merge decisions, cadence,
  and capture-to-sink orchestration;
- `screenpipe-audio`: opt-in WASAPI capture, VAD utterance segmentation, and
  local transcription;
- `screenpipe-cli`: the minimal `run`, `doctor`, and `service` command surface.

There is no desktop UI, cloud sync, marketplace, updater, SQLite runtime,
summarization, embedding, MCP server, or model-based merge decision.

`screenpipe-memory` 0.2 is a source-breaking compatibility release: its public
`SplitReason` enum adds `TimestampRegression`, and its persisted merge contract
is version 7. Exhaustive downstream matches written for 0.1 must handle the new
variant.

## Build and offline verification

The audio commands use a one-time online bootstrap for pinned Ninja 1.12.1
unless that exact binary is already present; verification itself stays local.

```powershell
cargo fmt --all -- --check
cargo test --workspace --exclude screenpipe-audio
cargo check --workspace --exclude screenpipe-audio

$ninjaVersion = '1.12.1'
$ninjaRoot = Join-Path $env:LOCALAPPDATA "screen-memory\tools\ninja\$ninjaVersion"
$ninja = Join-Path $ninjaRoot 'ninja.exe'
if (-not (Test-Path -LiteralPath $ninja)) {
    $archive = Join-Path $env:TEMP "ninja-$ninjaVersion-win.zip"
    $uri = "https://github.com/ninja-build/ninja/releases/download/v$ninjaVersion/ninja-win.zip"
    Invoke-WebRequest -Uri $uri -OutFile $archive
    New-Item -ItemType Directory -Path $ninjaRoot -Force | Out-Null
    Expand-Archive -LiteralPath $archive -DestinationPath $ninjaRoot -Force
}
$env:PATH = "$ninjaRoot;$env:PATH"
$env:CMAKE_GENERATOR = 'Ninja'
if ((& $ninja --version).Trim() -ne $ninjaVersion) {
    throw "Ninja $ninjaVersion is required"
}
$env:LIBCLANG_PATH = "$env:LOCALAPPDATA\screen-memory\llvm\bin"
cargo test -p screenpipe-audio
cargo check -p screenpipe-cli --features audio
powershell -NoProfile -File .\scripts\verify-pruned.ps1
```

Audio is a workspace member but not a default member, so the ordinary build
stays free of the whisper.cpp toolchain. CI is configured not to omit it: a
dedicated Windows job installs LLVM/libclang 18.1.8 and Ninja 1.12.1, sets
`CMAKE_GENERATOR=Ninja`, tests the audio crate, and checks the CLI with its
`audio` feature enabled. Hosted success remains a pull-request check rather
than a claim made by this document.

The three interactive Windows tests remain ignored by default. They require an
unlocked desktop with a controlled foreground window and must not be treated as
live proof merely because the normal workspace tests pass.

The PostgreSQL integration suite (`postgres_writer`) reads its own variable,
`SCREEN_MEMORY_TEST_DATABASE_URL`, and skips with a single `SKIPPED
postgres_writer:` line when it is unset - so the command above passes with no
environment at all, and a skipped run is never mistaken for a run that
exercised the database. The variable is separate from the agent's
`SCREEN_MEMORY_DATABASE_URL`, with no fallback to it, because the suite issues
`CREATE SCHEMA` and `DROP SCHEMA CASCADE` and Doppler injects the live capture
database. Whatever URL is supplied must still name a database whose name ends
in `_test`; anything else is refused. To run it against the disposable
database:

```powershell
$env:SCREEN_MEMORY_TEST_DATABASE_URL = 'postgresql://<user>:<password>@<host>:5432/screen_memory_test'
cargo test --workspace
```

## Clipboard channel

Text the operator copies is recorded as `events` of kind `clipboard`, in the
same table, under the same `{slug}_{seq}` identifiers, with the same
`merge_meta` document as screen events. **It is ON by default**; turn it off
with:

```powershell
doppler run -p homelab -c dev_personal -- .\target\release\screenpipe.exe run --no-clipboard
```

What it records, and what it refuses:

- **Text only.** `CF_UNICODETEXT` or nothing. An image, a file list, or a
  private application format is ignored entirely - no event, and no capture
  gap either, because nothing was lost.
- **Never a refused clipboard.** If the owning application sets
  `ExcludeClipboardContentFromMonitorProcessing`, or sets
  `CanIncludeInClipboardHistory` or `CanUploadToCloudClipboard` to `0` - which
  is what password managers do - the text is never read at all, and the run
  log records only `event=clipboard_excluded reason=owner_refused`. A
  permission format that is present but unreadable is treated as a refusal.
- **The first poll never reads.** Whatever is on the clipboard at startup was
  copied before the channel existed, so capturing it would re-record the same
  stale text on every service restart. The first poll learns where the
  clipboard is; only changes observed while running are captured.
- **Detection is a sequence-number poll** on a two-second tick
  (`GetClipboardSequenceNumber`), not `AddClipboardFormatListener`, which
  would need a hidden window and a message pump this process does not have.
- **A locked workstation captures nothing.** Windows refuses `OpenClipboard`
  to the default desktop while the session is locked, though the sequence
  number stays readable - so a locked machine polls a counter that never
  moves, and a copy made just before the lock is captured after it, not lost.

Merge discipline, deterministic and per kind: consecutive captures of the same
normalized text merge into the open event and raise its `sample_count`, so
re-copying the same thing does not write a second row saying the same thing.
Different text closes the event and opens a new one
(`start_reason = text_hash_change`), and the shared idle-gap threshold closes
it too. Scroll overlap is deliberately not consulted - two clipboard entries
that share most of their words are two copies of two different things, not one
thing being scrolled past.

A clipboard event has no application: `events.app_id` is NULL and no `apps`
row is created, because the foreground window at poll time is not reliably
where the copy came from.

## Audio channel

Speech is transcribed locally and recorded as `events` of kind `audio`, in the
same table, under the same `{slug}_{seq}` identifiers, through the same writer.
**It is OFF**, and it is off in two independent ways:

1. **It is not in this binary.** `audio` is a Cargo feature, disabled by
   default. Without it there is no capture path, no whisper model, and no code
   that can open a microphone - and the build needs neither CMake nor libclang.
2. **Even in a build that has it, it records nothing until it is started.**
   `screenpipe run` does not start it and `screenpipe service install` does not
   install it. It has its own subcommand and its own scheduled task.

```powershell
# A build that CAN record audio. Uses the pinned Ninja setup above and needs
# libclang for whisper.cpp.
$ninjaVersion = '1.12.1'
$ninjaRoot = Join-Path $env:LOCALAPPDATA "screen-memory\tools\ninja\$ninjaVersion"
$env:PATH = "$ninjaRoot;$env:PATH"
$env:CMAKE_GENERATOR = 'Ninja'
if ((& (Join-Path $ninjaRoot 'ninja.exe') --version).Trim() -ne $ninjaVersion) {
    throw "Ninja $ninjaVersion is required; run the offline verification setup first"
}
$env:LIBCLANG_PATH = "$env:LOCALAPPDATA\screen-memory\llvm\bin"
cargo build --release -p screenpipe-cli --features audio

# Prove the endpoint, the model and the database work. Records nothing.
doppler run -p homelab -c dev_personal -- .\target\release\screenpipe.exe audio doctor

# Record, in the foreground, until Ctrl-C.
doppler run -p homelab -c dev_personal -- .\target\release\screenpipe.exe audio run
```

The model is a ggml whisper file, `ggml-base.en.bin` by default, looked for at
`%LOCALAPPDATA%\screen-memory\models\`, overridable with
`SCREEN_MEMORY_WHISPER_MODEL` or `--model`. Nothing downloads it automatically:
a channel that is off by default has no business fetching 141 MB on its own.

### What it records, and what it refuses

- **System audio, not the microphone.** The default is loopback: what came back
  through the speakers. It hears the far side of a call and not the operator's
  own voice. `--microphone` records the room instead, and everyone audible in
  it; that is a second, separate decision, it is never implied by turning the
  channel on, and every run that does it prints
  `event=audio_microphone state=on` at the top of the log.
- **Never a device name.** Windows exposes strings like `Microphone (Realtek
  High Definition Audio)`, which identify hardware in a particular person's
  house. None of them are read. What is recorded is the endpoint ROLE -
  `console` or `communications` - which says whether this stream follows the
  device a call would actually use.
- **Never near-silence.** Whisper does not return nothing for nothing; it
  returns its best guess, which over room tone is a caption artefact. Audio
  shorter than 250 ms is not transcribed at all, segments the model itself
  scores above 0.6 no-speech are dropped, and an utterance with nothing left
  writes no row - it logs `event=audio_discarded reason=no_speech`.
- **Never the transcript in a log line.** whisper.cpp will print segments to
  stdout if asked; every one of those switches is off, and the channel's own
  diagnostics are fixed strings that carry no length, timing, or hash of what
  was said.

### Boundaries

Silence decides. A WebRTC voice-activity detector runs on 20 ms frames: three
voiced frames open an utterance, 600 ms of silence closes it, the 200 ms before
the trigger is kept so the first consonant is not clipped, and the trailing
silence is cut before the audio reaches the model. Whisper is never asked where
speech starts.

Each separately closed utterance window is its own event, even when its
normalized transcript is identical to an earlier one. People can say the same
thing twice, and both occurrences must remain durable. Deduplication is scoped
to one VAD identity: only chunks with the same utterance start/end window and
the same normalized transcript merge. Near-silence hallucinations are rejected
by the no-speech filters above rather than erased by cross-utterance content
deduplication.

An audio event has an application, unlike a clipboard event: `audio:loopback`
or `audio:microphone`, titled `System Audio` or `Microphone`. `window_title` is
empty - attaching the foreground window would need a live channel to the screen
recorder that does not exist, and a guess would put `Zoom` on a video playing in
a browser.

`merge_meta.audio` carries the engine, the model's file stem, the VAD and its
sensitivity, the endpoint role, why the utterance ended, and the model's own
mean no-speech probability in parts per thousand. Screen and clipboard rows
carry no `audio` key at all.

An audio event's window is real: `ended_at - started_at` is how long the speech
ran. That takes a deliberate mechanism, because every other observation in this
system happens at an instant - a frame is read at a moment, a copy happens at a
moment - and an utterance runs for seconds. `ObservationEnvelope` carries that
end beside the source-compatible `ObservationSample`, and the merger measures
the idle gap from it. Without it the "silence" between two turns would include
the length of the first one, and a 30-second sentence followed by a 35-second
pause would split at a 60-second threshold that 35 seconds of silence never
crossed.

### Three threads, and why

Capture owns the WASAPI stream and must never block: the audio engine's buffer
is finite, and a stalled reader loses audio with no record that it happened. It
hands closed utterances to a transcription thread and, if that thread is more
than eight utterances behind, drops them and says so
(`event=audio_dropped reason=transcriber_backlog`) rather than wait. The
transcription thread owns the model and is the expensive one. The async loop
owns the database. Dropping happens at the cheap end, never at the durable one.

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

### TLS

The database is the CloudNativePG cluster reached over the LAN at
`pg18-core-db.moosegoose.xyz`, which resolves to `192.168.30.205`. `sqlx` is
built with `tls-native-tls`, and the live connection string uses
`sslmode=verify-full` plus `sslrootcert`. SQLx adds that CA directly to the
connector and validates both the server certificate chain and the hostname.
No machine-wide Windows trust-store change is required.

The hostname is covered by the CNPG server certificate's SANs. Connecting with
`verify-full` to the bare `192.168.30.205` is intentionally rejected because
the certificate does not contain that IP, so the hostname check is not merely
configured but enforced. The CNPG CA expires on `2026-10-21`; re-export it
before then with the homelab repository's `ops/Sync-ClusterDbTrust.ps1`
procedure.

The previous IP-only configuration used `sslmode=verify-ca`, which validates
the CA chain while skipping only hostname verification. That remains valid
compatibility behavior for deployments without a certificate-covered DNS name,
but do not replace it with `require`: `require` would keep encryption while
removing certificate validation. The live endpoint now supports the stronger
`verify-full` mode.

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
bar. Clipboard text is read only from generations whose owner did not refuse
monitoring, and reaches only the `events` row, exactly as OCR text does. Logs
and debug output do not include OCR bodies, readable text, clipboard text or
its length or hash, URL query strings, secret values, frame bytes, or frame
fingerprints.
