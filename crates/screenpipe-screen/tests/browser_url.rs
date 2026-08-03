use screenpipe_screen::{
    BrowserUrlReader, ForegroundMetadata, UiaElementSnapshot, select_address_bar,
};
use url::Url;

const EDIT_CONTROL: i32 = 50_004;
const BUTTON_CONTROL: i32 = 50_000;

fn button() -> UiaElementSnapshot {
    UiaElementSnapshot {
        control_type: BUTTON_CONTROL,
        name: "Reload this page".to_owned(),
        automation_id: "reload-button".to_owned(),
        value: None,
        is_enabled: true,
        is_offscreen: false,
        has_document_ancestor: false,
    }
}

fn address_bar(value: Option<&str>) -> UiaElementSnapshot {
    UiaElementSnapshot {
        control_type: EDIT_CONTROL,
        name: "Address and search bar".to_owned(),
        automation_id: "view_1022".to_owned(),
        value: value.map(str::to_owned),
        is_enabled: true,
        is_offscreen: false,
        has_document_ancestor: false,
    }
}

fn page_input(name: &str, automation_id: &str, value: &str) -> UiaElementSnapshot {
    UiaElementSnapshot {
        control_type: EDIT_CONTROL,
        name: name.to_owned(),
        automation_id: automation_id.to_owned(),
        value: Some(value.to_owned()),
        is_enabled: true,
        is_offscreen: false,
        has_document_ancestor: true,
    }
}

#[test]
fn selects_the_enabled_visible_chrome_address_bar() {
    let elements = vec![
        button(),
        address_bar(Some("https://example.test/chrome?q=1")),
    ];

    assert_eq!(
        select_address_bar("CHROME.EXE", &elements),
        Some(Url::parse("https://example.test/chrome?q=1").unwrap())
    );
}

#[test]
fn selects_the_enabled_visible_edge_address_bar() {
    let elements = vec![
        UiaElementSnapshot {
            control_type: EDIT_CONTROL,
            name: "Search".to_owned(),
            automation_id: "toolbar-search".to_owned(),
            value: Some("not a URL".to_owned()),
            is_enabled: true,
            is_offscreen: false,
            has_document_ancestor: false,
        },
        address_bar(Some("https://github.com/MooseGooseConsulting/screenpipe")),
    ];

    assert_eq!(
        select_address_bar("msedge.exe", &elements),
        Some(Url::parse("https://github.com/MooseGooseConsulting/screenpipe").unwrap())
    );
}

#[test]
fn skips_url_looking_page_inputs_before_the_browser_address_bar() {
    let elements = vec![
        page_input(
            "Email address",
            "email-address",
            "https://accounts.example.test/private-profile",
        ),
        page_input(
            "Delivery destination",
            "shipping-address",
            "https://orders.example.test/private-order",
        ),
        page_input(
            "Address and search bar",
            "page-location",
            "https://search.example.test/private-search",
        ),
        address_bar(Some("https://example.test/safe-browser-location")),
    ];

    assert_eq!(
        select_address_bar("chrome.exe", &elements).map(|url| url.path().to_owned()),
        Some("/safe-browser-location".to_owned())
    );
}

#[test]
fn rejects_page_inputs_with_address_like_names_and_identifiers() {
    let elements = vec![
        page_input(
            "Email address",
            "customer-email",
            "https://accounts.example.test/private-profile",
        ),
        page_input(
            "Shipping destination",
            "shipping-address",
            "https://orders.example.test/private-order",
        ),
        page_input(
            "Address and search bar for profile",
            "profile-location",
            "https://profile.example.test/private-profile",
        ),
        page_input(
            "Address and search bar",
            "page-location",
            "https://search.example.test/private-search",
        ),
    ];

    assert!(select_address_bar("msedge.exe", &elements).is_none());
}

#[test]
fn rejects_an_exact_address_bar_collision_under_page_document() {
    let exact_collision = page_input(
        "Address and search bar",
        "view_1022",
        "https://page.example.test/private-page-value",
    );

    assert!(select_address_bar("chrome.exe", &[exact_collision]).is_none());
}

#[test]
fn ignores_address_bar_snapshots_for_non_browsers() {
    let elements = vec![address_bar(Some("https://example.test"))];

    assert_eq!(select_address_bar("notepad.exe", &elements), None);
}

#[test]
fn rejects_a_disabled_or_offscreen_address_bar() {
    let mut disabled = address_bar(Some("https://example.test/disabled"));
    disabled.is_enabled = false;
    let mut offscreen = address_bar(Some("https://example.test/offscreen"));
    offscreen.is_offscreen = true;

    assert_eq!(
        select_address_bar("chrome.exe", &[disabled, offscreen]),
        None
    );
}

#[test]
fn rejects_malformed_and_non_http_address_values() {
    let malformed = address_bar(Some("not a URL"));
    let file_url = address_bar(Some("file:///C:/private.txt"));

    assert_eq!(
        select_address_bar("chrome.exe", &[malformed, file_url]),
        None
    );
}

#[test]
fn returns_none_when_a_browser_has_no_address_bar_value() {
    let elements = vec![button(), address_bar(None)];

    assert_eq!(select_address_bar("msedge.exe", &elements), None);
}

#[test]
fn reader_never_opens_uia_for_a_non_browser() {
    let metadata = ForegroundMetadata {
        window_handle: 0,
        app_key: "notepad.exe".to_owned(),
        app_title: "Notepad".to_owned(),
        window_title: "notes".to_owned(),
        browser_url: None,
    };

    assert_eq!(
        BrowserUrlReader.read_for_foreground(&metadata).unwrap(),
        None
    );
}

#[test]
fn reader_treats_a_stale_browser_window_as_optional_metadata() {
    let metadata = ForegroundMetadata {
        window_handle: 0,
        app_key: "chrome.exe".to_owned(),
        app_title: "Google Chrome".to_owned(),
        window_title: "No longer foreground".to_owned(),
        browser_url: None,
    };

    assert!(matches!(
        BrowserUrlReader.read_for_foreground(&metadata),
        Ok(None)
    ));
}

#[test]
fn snapshot_debug_output_redacts_the_uia_value() {
    let page_value =
        "https://accounts.example.test/UIA_PAGE_VALUE_SECRET_66d9?token=UIA_QUERY_SECRET_a4e1";
    let snapshot = page_input("Email address", "email-address", page_value);
    let debug = format!("{snapshot:?}");

    assert!(!debug.contains(page_value));
    assert!(!debug.contains("UIA_PAGE_VALUE_SECRET_66d9"));
    assert!(!debug.contains("UIA_QUERY_SECRET_a4e1"));
    assert!(debug.contains("<redacted>"));
}

#[test]
fn foreground_metadata_debug_redacts_browser_url_path_and_query() {
    let browser_url = "https://browser.example.test/FOREGROUND_PATH_SECRET_b82c?token=FOREGROUND_QUERY_SECRET_517a";
    let metadata = ForegroundMetadata {
        window_handle: 42,
        app_key: "chrome.exe".to_owned(),
        app_title: "Google Chrome".to_owned(),
        window_title: "Project notes".to_owned(),
        browser_url: Some(browser_url.to_owned()),
    };
    let debug = format!("{metadata:?}");

    assert!(!debug.contains(browser_url));
    assert!(!debug.contains("FOREGROUND_PATH_SECRET_b82c"));
    assert!(!debug.contains("FOREGROUND_QUERY_SECRET_517a"));
    assert!(debug.contains("chrome.exe"));
    assert!(debug.contains("Project notes"));
    assert!(debug.contains("<redacted>"));
}
