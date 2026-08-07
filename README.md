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

### TLS, and a downgrade you should know about

The database is a CloudNativePG cluster reached over the LAN by IP, so the
connection is TLS. `sqlx` is built with `tls-rustls-ring`: rustls with the
`ring` provider and the bundled Mozilla root set, plus whatever `sslrootcert`
names. Deliberately not `native-tls` and not `tls-rustls-ring-native-roots` -
both read the Windows certificate store on Windows, so trusting this cluster
would have meant a machine-wide trust change. Nothing outside this process is
affected by what it trusts.

**`sslmode=verify-ca` does not work, and the recorder downgrades to `require`
when it is asked for.** sqlx 0.8.6's `NoHostnameTlsVerifier` skips the hostname
check by swallowing one rustls error, `CertificateError::NotValidForName`.
rustls 0.23 replaced that with `NotValidForNameContext` - a different variant -
so the arm never fires and the name check cannot be skipped. Our server
certificate carries in-cluster DNS SANs and no IP SAN, and we connect by IP, so
that check can never pass.

The fallback is conditional, not a rewrite: `verify-ca` is attempted on every
start, and the downgrade happens only when that attempt failed for the name
check specifically. An untrusted issuer, an expired certificate, or a server
refusing TLS still fails hard. When it does downgrade it says so, once, on
stderr and therefore in the agent log:

```
WARNING event=tls_downgrade requested=verify-ca effective=require ...
```

While downgraded, traffic is encrypted but the server certificate is not
verified, so a LAN attacker able to intercept the connection is not excluded.
The clean fix is to stop connecting by IP: give the host a name that is already
in the certificate's SANs, resolvable to the cluster address, and use
`sslmode=verify-full`, which this build honours fully. That is a change to the
Doppler secret and to DNS, not to this code.

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
