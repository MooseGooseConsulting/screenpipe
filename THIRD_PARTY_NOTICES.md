# Third-party notices

## xcap 0.9.4

`vendor/xcap` is a source-controlled derivative of xcap 0.9.4, used only for
Windows window capture. It is licensed under Apache-2.0. Its unmodified
license text is retained at `vendor/xcap/LICENSE`.

The only local modifications are in `vendor/xcap/src/windows/capture.rs` and
`vendor/xcap/src/windows/wgc.rs`: window capture uses the actual
`GraphicsCaptureItem` dimensions instead of a DPI-derived DWM rectangle and
returns window callback failures to its caller. This prevents an out-of-bounds
ROI from being misreported as a WGC timeout for DPI-unaware windows.
Monitor-region capture is unchanged.
