use screenpipe_screen::{
    BrowserUrlReader, ForegroundMetadata, UiaElementSnapshot, select_browser_url,
};
use url::Url;

const DOCUMENT_CONTROL: i32 = 50_030;
const EDIT_CONTROL: i32 = 50_004;
const GROUP_CONTROL: i32 = 50_026;

/// The browser's own top-level document node: the only thing whose UIA value is
/// the committed URL.
fn top_level_document(value: Option<&str>) -> UiaElementSnapshot {
    UiaElementSnapshot {
        control_type: DOCUMENT_CONTROL,
        name: "Example page".to_owned(),
        automation_id: "RootWebArea".to_owned(),
        value: value.map(str::to_owned),
        is_enabled: true,
        is_offscreen: false,
        has_document_ancestor: false,
    }
}

/// A node living under a page document. Anything shaped like this is page
/// content, whatever it calls itself.
fn under_page_document(control_type: i32, automation_id: &str, value: &str) -> UiaElementSnapshot {
    UiaElementSnapshot {
        control_type,
        name: "Page content".to_owned(),
        automation_id: automation_id.to_owned(),
        value: Some(value.to_owned()),
        is_enabled: true,
        is_offscreen: false,
        has_document_ancestor: true,
    }
}

#[test]
fn selects_the_top_level_document_url_for_chrome_and_edge() {
    // The whole point of the migration: this fixture is what a browser sitting
    // on an ordinary page publishes in steady state, with nobody touching the
    // omnibox. The retired path returned nothing here, because the omnibox's
    // displayed text has had its scheme elided since Chromium M69.
    for app_key in ["chrome.exe", "CHROME.EXE", "msedge.exe", "MsEdge.exe"] {
        assert_eq!(
            select_browser_url(
                app_key,
                &[top_level_document(Some("https://example.test/page?q=1"))],
            ),
            Some(Url::parse("https://example.test/page?q=1").unwrap()),
            "{app_key}"
        );
    }
}

#[test]
fn rejects_a_page_node_impersonating_the_document_automation_id() {
    // A page author controls the HTML `id` attribute, and Chromium derives a
    // node's automation id from it. A page that names an element
    // `RootWebArea` - even one Chromium exposes as a Document, which ARIA
    // `role="document"` does not - is still page content, and its provenance
    // is what says so.
    let impersonators = [
        (
            "a Document-typed page node",
            under_page_document(DOCUMENT_CONTROL, "RootWebArea", "https://leaked.test/page"),
        ),
        (
            "an ARIA role=document group",
            under_page_document(GROUP_CONTROL, "RootWebArea", "https://leaked.test/aria"),
        ),
        (
            "a page input claiming the id",
            under_page_document(EDIT_CONTROL, "RootWebArea", "https://leaked.test/input"),
        ),
    ];

    for (description, impersonator) in impersonators {
        assert_eq!(
            select_browser_url("chrome.exe", std::slice::from_ref(&impersonator)),
            None,
            "{description} must never be reported as the page URL"
        );
    }
}

#[test]
fn rejects_nodes_that_miss_either_half_of_the_document_identity() {
    // The identity is an AND of control type and automation id, so each decoy
    // independently pins one half. Every one of these sits OUTSIDE any
    // document, so only the identity check can reject it.
    let decoys = [
        (
            "the control type is not Document",
            UiaElementSnapshot {
                control_type: EDIT_CONTROL,
                value: Some("https://leaked.test/edit".to_owned()),
                ..top_level_document(None)
            },
        ),
        (
            "the automation id is not the Chromium constant",
            UiaElementSnapshot {
                automation_id: "view_1012".to_owned(),
                value: Some("https://leaked.test/omnibox".to_owned()),
                ..top_level_document(None)
            },
        ),
        (
            "the automation id differs only in case",
            UiaElementSnapshot {
                automation_id: "rootwebarea".to_owned(),
                value: Some("https://leaked.test/case".to_owned()),
                ..top_level_document(None)
            },
        ),
    ];

    for (description, decoy) in decoys {
        assert_eq!(
            select_browser_url("chrome.exe", std::slice::from_ref(&decoy)),
            None,
            "a node where {description} must never be reported as the page URL"
        );
    }
}

#[test]
fn two_top_level_documents_yield_absent_rather_than_a_guess() {
    // Edge split-screen, a docked DevTools window, and side panels each publish
    // their own top-level document. Nothing in the tree says which one the user
    // is looking at, and a browser event carrying the wrong URL is worse than
    // one carrying none.
    let elements = vec![
        top_level_document(Some("https://left.example.test/pane")),
        top_level_document(Some("https://right.example.test/pane")),
    ];

    assert_eq!(select_browser_url("msedge.exe", &elements), None);
}

#[test]
fn an_offscreen_document_is_not_the_focused_tab() {
    // A background tab's document stays in the tree and keeps its URL.
    let elements = vec![
        UiaElementSnapshot {
            is_offscreen: true,
            ..top_level_document(Some("https://background.example.test/tab"))
        },
        top_level_document(Some("https://foreground.example.test/tab")),
    ];

    assert_eq!(
        select_browser_url("chrome.exe", &elements).map(|url| url.host_str().unwrap().to_owned()),
        Some("foreground.example.test".to_owned())
    );
}

#[test]
fn a_nested_frame_document_never_wins_over_the_top_level_one() {
    // Out-of-process iframe roots are themselves `kRootWebArea` and carry the
    // same automation id. Only provenance separates them, and the ordering here
    // proves it is provenance rather than position: the frame comes first.
    let elements = vec![
        under_page_document(
            DOCUMENT_CONTROL,
            "RootWebArea",
            "https://ads.example.test/iframe",
        ),
        top_level_document(Some("https://example.test/real-page")),
    ];

    assert_eq!(
        select_browser_url("chrome.exe", &elements),
        Some(Url::parse("https://example.test/real-page").unwrap())
    );
}

#[test]
fn ignores_document_snapshots_for_non_browsers() {
    let elements = vec![top_level_document(Some("https://example.test"))];

    assert_eq!(select_browser_url("notepad.exe", &elements), None);
}

#[test]
fn rejects_every_non_http_scheme_a_browser_actually_shows() {
    // These are real top-level documents with real values. Refusing them is a
    // policy decision, not a parse failure: a memory index has no use for the
    // inspector's own URL, and `file://` is a local path.
    for value in [
        "devtools://devtools/bundled/inspector.html",
        "chrome://settings/privacy",
        "edge://settings/privacy",
        "chrome-extension://abcdefghijklmnop/options.html",
        "file:///C:/private.txt",
        "about:blank",
    ] {
        assert_eq!(
            select_browser_url("chrome.exe", &[top_level_document(Some(value))]),
            None,
            "{value} must not be recorded as a browser URL"
        );
    }
}

#[test]
fn returns_none_when_the_document_has_no_readable_value() {
    // Tier 3 of the ladder. Absent is legal - the event carries app and window
    // title only - and must never be papered over by reconstructing a scheme.
    for value in [None, Some(""), Some("   "), Some("example.test/elided")] {
        assert_eq!(
            select_browser_url("msedge.exe", &[top_level_document(value)]),
            None,
            "{value:?} must not become a URL"
        );
    }
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
    let snapshot = under_page_document(EDIT_CONTROL, "email-address", page_value);
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
