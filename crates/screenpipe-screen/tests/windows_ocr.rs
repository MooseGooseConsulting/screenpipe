#![cfg(target_os = "windows")]

use std::ffi::c_void;

use screenpipe_screen::{TransientFrame, WindowsOcr};

#[repr(C)]
struct BitmapInfoHeader {
    size: u32,
    width: i32,
    height: i32,
    planes: u16,
    bit_count: u16,
    compression: u32,
    size_image: u32,
    x_pels_per_meter: i32,
    y_pels_per_meter: i32,
    clr_used: u32,
    clr_important: u32,
}

#[repr(C)]
struct RgbQuad {
    blue: u8,
    green: u8,
    red: u8,
    reserved: u8,
}

#[repr(C)]
struct BitmapInfo {
    header: BitmapInfoHeader,
    colors: [RgbQuad; 1],
}

#[link(name = "user32")]
unsafe extern "system" {
    fn GetDC(window: *mut c_void) -> *mut c_void;
    fn ReleaseDC(window: *mut c_void, dc: *mut c_void) -> i32;
}

#[link(name = "gdi32")]
unsafe extern "system" {
    fn CreateCompatibleDC(dc: *mut c_void) -> *mut c_void;
    fn DeleteDC(dc: *mut c_void) -> i32;
    fn CreateDIBSection(
        dc: *mut c_void,
        info: *const BitmapInfo,
        usage: u32,
        bits: *mut *mut c_void,
        section: *mut c_void,
        offset: u32,
    ) -> *mut c_void;
    fn SelectObject(dc: *mut c_void, object: *mut c_void) -> *mut c_void;
    fn DeleteObject(object: *mut c_void) -> i32;
    fn PatBlt(dc: *mut c_void, x: i32, y: i32, width: i32, height: i32, operation: u32) -> i32;
    fn SetTextColor(dc: *mut c_void, color: u32) -> u32;
    fn SetBkMode(dc: *mut c_void, mode: i32) -> i32;
    fn CreateFontW(
        height: i32,
        width: i32,
        escapement: i32,
        orientation: i32,
        weight: i32,
        italic: u32,
        underline: u32,
        strike_out: u32,
        char_set: u32,
        output_precision: u32,
        clip_precision: u32,
        quality: u32,
        pitch_and_family: u32,
        face: *const u16,
    ) -> *mut c_void;
    fn TextOutW(dc: *mut c_void, x: i32, y: i32, text: *const u16, length: i32) -> i32;
}

fn render_fixture() -> (u32, u32, Vec<u8>) {
    const WIDTH: i32 = 1600;
    const HEIGHT: i32 = 260;
    const BI_RGB: u32 = 0;
    const DIB_RGB_COLORS: u32 = 0;
    const WHITENESS: u32 = 0x00ff_0062;
    const TRANSPARENT: i32 = 1;
    const FW_BOLD: i32 = 700;
    const CLEARTYPE_QUALITY: u32 = 5;

    let info = BitmapInfo {
        header: BitmapInfoHeader {
            size: size_of::<BitmapInfoHeader>() as u32,
            width: WIDTH,
            height: -HEIGHT,
            planes: 1,
            bit_count: 32,
            compression: BI_RGB,
            size_image: (WIDTH * HEIGHT * 4) as u32,
            x_pels_per_meter: 0,
            y_pels_per_meter: 0,
            clr_used: 0,
            clr_important: 0,
        },
        colors: [RgbQuad {
            blue: 0,
            green: 0,
            red: 0,
            reserved: 0,
        }],
    };
    let face = "Segoe UI\0".encode_utf16().collect::<Vec<_>>();
    let text = "GOAL ONE VISION OCR".encode_utf16().collect::<Vec<_>>();

    unsafe {
        let screen_dc = GetDC(std::ptr::null_mut());
        assert!(!screen_dc.is_null());
        let memory_dc = CreateCompatibleDC(screen_dc);
        assert!(!memory_dc.is_null());
        let mut bits = std::ptr::null_mut();
        let bitmap = CreateDIBSection(
            screen_dc,
            &info,
            DIB_RGB_COLORS,
            &mut bits,
            std::ptr::null_mut(),
            0,
        );
        assert!(!bitmap.is_null());
        assert!(!bits.is_null());
        let old_bitmap = SelectObject(memory_dc, bitmap);
        assert_ne!(PatBlt(memory_dc, 0, 0, WIDTH, HEIGHT, WHITENESS), 0);
        let font = CreateFontW(
            -120,
            0,
            0,
            0,
            FW_BOLD,
            0,
            0,
            0,
            1,
            0,
            0,
            CLEARTYPE_QUALITY,
            0,
            face.as_ptr(),
        );
        assert!(!font.is_null());
        let old_font = SelectObject(memory_dc, font);
        SetTextColor(memory_dc, 0);
        SetBkMode(memory_dc, TRANSPARENT);
        assert_ne!(
            TextOutW(memory_dc, 35, 55, text.as_ptr(), text.len() as i32),
            0
        );

        let pixels =
            std::slice::from_raw_parts(bits.cast::<u8>(), (WIDTH * HEIGHT * 4) as usize).to_vec();
        SelectObject(memory_dc, old_font);
        SelectObject(memory_dc, old_bitmap);
        DeleteObject(font);
        DeleteObject(bitmap);
        DeleteDC(memory_dc);
        ReleaseDC(std::ptr::null_mut(), screen_dc);

        (WIDTH as u32, HEIGHT as u32, pixels)
    }
}

#[tokio::test(flavor = "current_thread")]
async fn recognizes_a_real_windows_drawn_bitmap() {
    let (width, height, pixels) = render_fixture();
    let frame = TransientFrame::from_bgra(width, height, width * 4, pixels).unwrap();

    let text = WindowsOcr.recognize(&frame).await.unwrap().to_lowercase();

    for expected in ["goal", "one", "vision", "ocr"] {
        assert!(text.contains(expected), "missing {expected:?} in {text:?}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_frame_wider_than_the_engine_accepts_is_still_recognized() {
    // Windows OCR refuses a bitmap whose longest side exceeds
    // `OcrEngine::MaxImageDimension`, and a display wide enough to produce one
    // produces it on every tick - so the refusal never clears and the recorder
    // recognises nothing for the life of the run while still paying for the
    // capture, the conversion and the encode each time.
    //
    // This drives the real engine, not the arithmetic: it is the assertion
    // that the scaled frame is actually accepted, which no unit test of the
    // scaling function can make.
    const WIDTH: u32 = 10_400;
    const HEIGHT: u32 = 160;
    // The fixture has to actually exceed the limit or this test passes
    // vacuously. 10,000 is the documented `MaxImageDimension` and what this
    // machine's engine reports; a compile-time bound means trimming WIDTH
    // fails the build rather than quietly emptying the test.
    const {
        assert!(
            WIDTH > 10_000,
            "the fixture no longer exceeds the OCR limit"
        );
    }
    let mut pixels = vec![0xFF_u8; (WIDTH * HEIGHT * 4) as usize];
    // One dark row, so the bitmap is not uniform and the encode has something
    // to do. The text this frame carries is irrelevant - being accepted at all
    // is the property under test.
    for pixel in pixels[..(WIDTH * 4) as usize].chunks_exact_mut(4) {
        pixel[..3].fill(0);
    }
    let frame = TransientFrame::from_bgra(WIDTH, HEIGHT, WIDTH * 4, pixels).unwrap();

    WindowsOcr
        .recognize(&frame)
        .await
        .expect("an oversized frame must be scaled to fit rather than failing forever");
}

#[test]
fn transient_frame_rejects_invalid_bgra_layouts() {
    assert!(TransientFrame::from_bgra(0, 1, 0, Vec::new()).is_err());
    assert!(TransientFrame::from_bgra(2, 1, 4, vec![0; 4]).is_err());
    assert!(TransientFrame::from_bgra(2, 2, 8, vec![0; 8]).is_err());
}
