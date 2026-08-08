# Local wasapi patch

This directory vendors `wasapi` 0.23.0 from crates.io, checksum
`80c3aa5d6b0e7acc3ea10cb19c334df0c8d825060f14a30d9e3b03385e6e5175`,
upstream commit `ce77ae1040128f0f92587adda0de75bc38eaa3db`.

The local change is intentionally limited to `Handle::wait_for_event` and its
error contract. Two trailing spaces in the upstream README were also removed
so the repository's whitespace check remains clean. The behavioral patch is:

- `WAIT_TIMEOUT` remains `WasapiError::EventTimeout`;
- `WAIT_FAILED` preserves the Windows error as `WasapiError::Windows`;
- any other non-signaled wait result becomes `UnexpectedWaitResult`.

Upstream 0.23.0 maps every result other than `WAIT_OBJECT_0` to
`EventTimeout`, which makes a broken wait handle indistinguishable from normal
silence. The regression test uses a null handle to exercise the real
`WAIT_FAILED` boundary without audio hardware.
