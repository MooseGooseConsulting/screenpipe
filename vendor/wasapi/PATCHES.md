# Local wasapi patch

This directory vendors `wasapi` 0.23.0 from crates.io, checksum
`80c3aa5d6b0e7acc3ea10cb19c334df0c8d825060f14a30d9e3b03385e6e5175`,
upstream commit `ce77ae1040128f0f92587adda0de75bc38eaa3db`.

The local changes are intentionally limited to `Handle::wait_for_event` and
capture-packet copying/release. Two trailing spaces in the
upstream README were also removed so the repository's whitespace check remains
clean. The behavioral patches are:

- `WAIT_TIMEOUT` remains `WasapiError::EventTimeout`;
- `WAIT_FAILED` preserves the Windows error as `WasapiError::Windows`;
- any other non-signaled wait result becomes `UnexpectedWaitResult`.
- a silent capture packet appends or copies exactly its frame length of zero
  bytes before any buffer pointer is inspected;
- every successful capture acquisition is released exactly once, including a
  zero-frame packet;
- a non-silent null capture pointer uses the pre-existing
  `DataLengthTooShort` error after the caller releases the WASAPI packet. No
  public `WasapiError` variants are added by this patch.

Upstream 0.23.0 maps every result other than `WAIT_OBJECT_0` to
`EventTimeout`, which makes a broken wait handle indistinguishable from normal
silence. The wait regression uses a null handle to exercise the real
`WAIT_FAILED` boundary without audio hardware.

WASAPI may legally return a null buffer pointer with nonzero frames when
`AUDCLNT_BUFFERFLAGS_SILENT` is set. The packet-copy regressions exercise that
contract without opening an endpoint, and keep the release call on both the
normal and rejected-copy paths.
