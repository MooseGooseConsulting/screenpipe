use std::fmt;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use uiautomation::controls::ControlType;
use uiautomation::core::UICondition;
use uiautomation::patterns::{UILegacyIAccessiblePattern, UIValuePattern};
use uiautomation::types::{Handle, TreeScope, UIProperty};
use uiautomation::variants::Variant;
use uiautomation::{UIAutomation, UIElement, UITreeWalker};
use url::Url;

use crate::ForegroundMetadata;
use crate::windows_metadata::foreground_window_handle;

const UIA_DOCUMENT_CONTROL_TYPE_ID: i32 = 50_030;

/// Chromium's automation id for the top-level document node of a tab.
///
/// Lives in shared `ui/accessibility`, so Chrome and Edge run the same code;
/// it is deliberately locale-independent and is not derived from anything a
/// page author can set. It replaces the `view_1012` / `view_1022` omnibox
/// match, which was `"view_" + View::GetID()` - an unnumbered enumerator whose
/// ordinal shifts whenever an entry is added to `chrome/browser/ui/view_ids.h`,
/// and which Edge forks outright - paired with an English-only accessible name.
const ROOT_WEB_AREA_AUTOMATION_ID: &str = "RootWebArea";

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiaElementSnapshot {
    pub control_type: i32,
    pub name: String,
    pub automation_id: String,
    pub value: Option<String>,
    pub is_enabled: bool,
    pub is_offscreen: bool,
    pub has_document_ancestor: bool,
}

impl fmt::Debug for UiaElementSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UiaElementSnapshot")
            .field("control_type", &self.control_type)
            .field("name", &self.name)
            .field("automation_id", &self.automation_id)
            .field("value", &self.value.as_ref().map(|_| "<redacted>"))
            .field("is_enabled", &self.is_enabled)
            .field("is_offscreen", &self.is_offscreen)
            .field("has_document_ancestor", &self.has_document_ancestor)
            .finish()
    }
}

/// The committed top-level URL of the focused tab, or `None`.
///
/// Reads the browser's *document* node, not its omnibox. The omnibox's UIA
/// value is the omnibox's displayed text, and Chromium's steady-state elision
/// removes the scheme from that text until the field is edited - so
/// `Url::parse` rejected it as a relative URL on every ordinary page, and only
/// an operator pressing Ctrl+L ever produced a value. The document node returns
/// `AXTreeData.url`, the browser's own committed URL, unelided and with its
/// scheme, in steady state.
pub fn select_browser_url(app_key: &str, elements: &[UiaElementSnapshot]) -> Option<Url> {
    select_browser_url_or_reason(app_key, elements).ok()
}

/// The same selection, carrying the fixed category that explains an absence.
///
/// Split from `select_browser_url` so the diagnostic can name which condition
/// failed without any caller gaining access to a value.
fn select_browser_url_or_reason(
    app_key: &str,
    elements: &[UiaElementSnapshot],
) -> Result<Url, &'static str> {
    if !is_supported_browser(app_key) {
        return Err("unsupported_app");
    }

    let mut top_level = elements
        .iter()
        .filter(|element| is_top_level_document(element));
    let document = top_level.next().ok_or("no_top_level_document")?;
    // Exactly one surviving candidate, or absent. Edge split-screen, docked
    // DevTools, and side panels each publish their own top-level document, and
    // nothing in the tree says which one the user is looking at.
    if top_level.next().is_some() {
        return Err("ambiguous_documents");
    }

    let value = document.value.as_deref().ok_or("value_unavailable")?;
    let url = Url::parse(value.trim()).map_err(|_| "value_unavailable")?;
    matches!(url.scheme(), "http" | "https")
        .then_some(url)
        .ok_or("non_http_scheme")
}

fn is_supported_browser(app_key: &str) -> bool {
    matches!(
        app_key.to_ascii_lowercase().as_str(),
        "chrome.exe" | "msedge.exe"
    )
}

/// Whether this node is the browser's own top-level document.
///
/// The automation id is compared exactly, not case-folded: it is a Chromium
/// constant, while a page element's automation id comes from its HTML `id`
/// attribute, so folding case would widen the match toward something a page can
/// choose. The document-ancestor rejection is what excludes out-of-process
/// iframe roots, which are themselves `kRootWebArea`; ARIA `role="document"`
/// maps to a role Chromium excludes from platform-document treatment, so it
/// never reaches here with this control type.
fn is_top_level_document(element: &UiaElementSnapshot) -> bool {
    element.control_type == UIA_DOCUMENT_CONTROL_TYPE_ID
        && element.automation_id == ROOT_WEB_AREA_AUTOMATION_ID
        && !element.is_offscreen
        && !element.has_document_ancestor
}

pub struct BrowserUrlReader;

impl BrowserUrlReader {
    pub fn read_for_foreground(&self, metadata: &ForegroundMetadata) -> Result<Option<Url>> {
        if !is_supported_browser(&metadata.app_key) {
            return Ok(None);
        }

        Ok(read_with_steps(
            &metadata.app_key,
            metadata.window_handle,
            is_still_foreground,
            || setup_uia_traversal(metadata.window_handle),
            enumerate_documents,
            read_document_candidates,
            emit_diagnostic,
        ))
    }
}

fn is_still_foreground(expected_handle: isize) -> bool {
    foreground_window_handle().ok() == Some(expected_handle)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BrowserUrlDiagnostic {
    stage: &'static str,
    reason: &'static str,
}

impl BrowserUrlDiagnostic {
    fn new(stage: &'static str, reason: &'static str) -> Self {
        Self { stage, reason }
    }

    fn uia_unavailable(stage: &'static str) -> Self {
        Self::new(stage, "uia_unavailable")
    }

    fn event_line(self) -> String {
        format!(
            "event=browser_url_unavailable stage={} reason={}",
            self.stage, self.reason
        )
    }
}

type UiaSetup = (UIAutomation, UIElement, UICondition, UITreeWalker, Vec<i32>);
type UiaEnumeration = (
    UIAutomation,
    UIElement,
    UITreeWalker,
    Vec<i32>,
    Vec<UIElement>,
);

fn setup_uia_traversal(window_handle: isize) -> Result<UiaSetup, BrowserUrlDiagnostic> {
    let unavailable = || BrowserUrlDiagnostic::uia_unavailable("setup");
    let automation = UIAutomation::new().map_err(|_| unavailable())?;
    let root = automation
        .element_from_handle(Handle::from(window_handle))
        .map_err(|_| unavailable())?;
    let root_runtime_id = root.get_runtime_id().map_err(|_| unavailable())?;
    // Both halves of the identity are pushed into the provider rather than
    // filtered here. The predecessor asked for every Edit in the subtree and
    // then read four properties off each candidate; this asks for the one node
    // that can answer.
    let control_type = automation
        .create_property_condition(
            UIProperty::ControlType,
            Variant::from(ControlType::Document as i32),
            None,
        )
        .map_err(|_| unavailable())?;
    let automation_id = automation
        .create_property_condition(
            UIProperty::AutomationId,
            Variant::from(ROOT_WEB_AREA_AUTOMATION_ID),
            None,
        )
        .map_err(|_| unavailable())?;
    let document_condition = automation
        .create_and_condition(control_type, automation_id)
        .map_err(|_| unavailable())?;
    let walker = automation
        .get_control_view_walker()
        .map_err(|_| unavailable())?;

    Ok((
        automation,
        root,
        document_condition,
        walker,
        root_runtime_id,
    ))
}

fn enumerate_documents(setup: UiaSetup) -> Result<UiaEnumeration, BrowserUrlDiagnostic> {
    let (automation, root, document_condition, walker, root_runtime_id) = setup;
    let documents = root
        .find_all(TreeScope::Subtree, &document_condition)
        .map_err(|_| BrowserUrlDiagnostic::uia_unavailable("enumeration"))?;
    Ok((automation, root, walker, root_runtime_id, documents))
}

fn read_document_candidates(
    enumeration: UiaEnumeration,
) -> Result<Vec<UiaElementSnapshot>, BrowserUrlDiagnostic> {
    let (_automation, _root, walker, root_runtime_id, documents) = enumeration;
    let mut snapshots = Vec::new();
    for element in documents {
        if let Some(snapshot) = read_candidate_after_provenance(
            || has_document_ancestor(&element, &root_runtime_id, &walker),
            || snapshot_candidate_identity(&element),
            || read_candidate_value(&element),
        )? {
            snapshots.push(snapshot);
        }
    }
    Ok(snapshots)
}

fn has_document_ancestor(
    element: &UIElement,
    root_runtime_id: &[i32],
    walker: &UITreeWalker,
) -> Result<bool, BrowserUrlDiagnostic> {
    let unavailable = || BrowserUrlDiagnostic::uia_unavailable("candidate_traversal");
    let mut current = element.clone();
    for _ in 0..128 {
        let parent = walker.get_parent(&current).map_err(|_| unavailable())?;
        let control_type = parent.get_control_type().map_err(|_| unavailable())?;
        if control_type == ControlType::Document {
            return Ok(true);
        }
        let runtime_id = parent.get_runtime_id().map_err(|_| unavailable())?;
        if runtime_id == root_runtime_id {
            return Ok(false);
        }
        current = parent;
    }

    Err(BrowserUrlDiagnostic::new(
        "candidate_traversal",
        "traversal_limit",
    ))
}

fn snapshot_candidate_identity(
    element: &UIElement,
) -> Result<UiaElementSnapshot, BrowserUrlDiagnostic> {
    let unavailable = || BrowserUrlDiagnostic::uia_unavailable("candidate_traversal");
    Ok(UiaElementSnapshot {
        control_type: element.get_control_type().map_err(|_| unavailable())? as i32,
        name: element.get_name().map_err(|_| unavailable())?,
        automation_id: element.get_automation_id().map_err(|_| unavailable())?,
        value: None,
        is_enabled: element.is_enabled().map_err(|_| unavailable())?,
        is_offscreen: element.is_offscreen().map_err(|_| unavailable())?,
        has_document_ancestor: false,
    })
}

/// Tier 1 then tier 2 of the fallback ladder: the same accessor reached through
/// two COM routes. Tier 3 is absence, which the selector produces on its own.
///
/// `TextPattern` is deliberately absent and must stay absent - it would read
/// page content through UIA and escalate Chromium into its inline-text-box
/// mode, which is neither cheap nor OCR-first.
fn read_candidate_value(element: &UIElement) -> Result<String, BrowserUrlDiagnostic> {
    let value_pattern = element
        .get_pattern::<UIValuePattern>()
        .and_then(|pattern| pattern.get_value())
        .ok()
        .filter(|value| !value.trim().is_empty());
    if let Some(value) = value_pattern {
        return Ok(value);
    }

    element
        .get_pattern::<UILegacyIAccessiblePattern>()
        .and_then(|pattern| pattern.get_value())
        .map_err(|_| BrowserUrlDiagnostic::uia_unavailable("candidate_traversal"))
}

fn read_candidate_after_provenance<P, I, V>(
    has_document_ancestor: P,
    read_identity: I,
    read_value: V,
) -> Result<Option<UiaElementSnapshot>, BrowserUrlDiagnostic>
where
    P: FnOnce() -> Result<bool, BrowserUrlDiagnostic>,
    I: FnOnce() -> Result<UiaElementSnapshot, BrowserUrlDiagnostic>,
    V: FnOnce() -> Result<String, BrowserUrlDiagnostic>,
{
    if has_document_ancestor()? {
        return Ok(None);
    }

    // Re-checked against the snapshot even though the UIA condition already
    // filtered on both properties: the condition is evaluated by the provider,
    // and this is the only check the tests can drive.
    let mut snapshot = read_identity()?;
    if snapshot.control_type != UIA_DOCUMENT_CONTROL_TYPE_ID
        || snapshot.automation_id != ROOT_WEB_AREA_AUTOMATION_ID
        || snapshot.is_offscreen
    {
        return Ok(None);
    }
    snapshot.value = Some(read_value()?);
    Ok(Some(snapshot))
}

fn read_with_steps<S, E, Setup, Enumerate, Candidates, Focus, Diagnostic>(
    app_key: &str,
    expected_handle: isize,
    mut is_foreground: Focus,
    setup: Setup,
    enumerate: Enumerate,
    read_candidates: Candidates,
    mut diagnose: Diagnostic,
) -> Option<Url>
where
    Setup: FnOnce() -> Result<S, BrowserUrlDiagnostic>,
    Enumerate: FnOnce(S) -> Result<E, BrowserUrlDiagnostic>,
    Candidates: FnOnce(E) -> Result<Vec<UiaElementSnapshot>, BrowserUrlDiagnostic>,
    Focus: FnMut(isize) -> bool,
    Diagnostic: FnMut(BrowserUrlDiagnostic),
{
    if !is_foreground(expected_handle) {
        diagnose(BrowserUrlDiagnostic::new(
            "before_traversal",
            "focus_changed",
        ));
        return None;
    }

    let setup = match setup() {
        Ok(setup) => setup,
        Err(diagnostic) => {
            diagnose(diagnostic);
            return None;
        }
    };
    let enumerated = match enumerate(setup) {
        Ok(enumerated) => enumerated,
        Err(diagnostic) => {
            diagnose(diagnostic);
            return None;
        }
    };
    if !is_foreground(expected_handle) {
        diagnose(BrowserUrlDiagnostic::new(
            "after_enumeration",
            "focus_changed",
        ));
        return None;
    }

    let snapshots = match read_candidates(enumerated) {
        Ok(snapshots) => snapshots,
        Err(diagnostic) => {
            diagnose(diagnostic);
            return None;
        }
    };
    if !is_foreground(expected_handle) {
        diagnose(BrowserUrlDiagnostic::new(
            "after_candidate_traversal",
            "focus_changed",
        ));
        return None;
    }

    match select_browser_url_or_reason(app_key, &snapshots) {
        Ok(url) => Some(url),
        Err(reason) => {
            diagnose(BrowserUrlDiagnostic::new("selection", reason));
            None
        }
    }
}

// Diagnostics go to stderr, matching the capture-gap diagnostics elsewhere in
// the workspace. Do not route these through `tracing` until a subscriber is
// actually installed: no crate in this workspace initializes one, so a
// `tracing::debug!` here is discarded at the dispatcher and the operator loses
// the only signal that the document selector has stopped resolving.
fn emit_diagnostic(diagnostic: BrowserUrlDiagnostic) {
    eprintln!("{}", diagnostic.event_line());
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};

    use super::*;

    fn top_level_document() -> UiaElementSnapshot {
        UiaElementSnapshot {
            control_type: UIA_DOCUMENT_CONTROL_TYPE_ID,
            name: "Example page".to_owned(),
            automation_id: ROOT_WEB_AREA_AUTOMATION_ID.to_owned(),
            value: Some("https://example.test/safe".to_owned()),
            is_enabled: true,
            is_offscreen: false,
            has_document_ancestor: false,
        }
    }

    fn read_with_focus_script(
        focus_results: &[bool],
        snapshots: Vec<UiaElementSnapshot>,
    ) -> (Option<Url>, Vec<BrowserUrlDiagnostic>) {
        let next_focus_result = Cell::new(0);
        let diagnostics = RefCell::new(Vec::new());
        let selected = read_with_steps(
            "chrome.exe",
            42,
            |_| {
                let index = next_focus_result.get();
                next_focus_result.set(index + 1);
                focus_results.get(index).copied().unwrap_or(false)
            },
            || Ok(()),
            Ok,
            |_| Ok(snapshots),
            |diagnostic| diagnostics.borrow_mut().push(diagnostic),
        );
        (selected, diagnostics.into_inner())
    }

    #[test]
    fn a_nested_document_reads_no_identity_or_value() {
        // An out-of-process iframe root is itself a `kRootWebArea` and carries
        // the same automation id as the top-level document. It is rejected on
        // provenance alone, before anything is read off it.
        let identity_read = Cell::new(false);
        let value_read = Cell::new(false);

        let selected = read_candidate_after_provenance(
            || Ok(true),
            || {
                identity_read.set(true);
                Ok(UiaElementSnapshot {
                    has_document_ancestor: true,
                    ..top_level_document()
                })
            },
            || {
                value_read.set(true);
                Ok("https://frame.example.test/private".to_owned())
            },
        )
        .unwrap();

        assert!(selected.is_none());
        assert!(!identity_read.get());
        assert!(!value_read.get());
    }

    #[test]
    fn an_untrusted_node_outside_a_document_is_never_value_read() {
        // The test above returns early on the provenance branch, so it never
        // reaches the identity filter that guards the value read. These
        // fixtures sit OUTSIDE any document, so only the identity check can
        // stop the read.
        for (description, untrusted) in [
            (
                "the control type is not Document",
                UiaElementSnapshot {
                    control_type: 50_004,
                    ..top_level_document()
                },
            ),
            (
                "the automation id is not the Chromium constant",
                UiaElementSnapshot {
                    automation_id: "rootwebarea".to_owned(),
                    ..top_level_document()
                },
            ),
            (
                "the node is offscreen",
                UiaElementSnapshot {
                    is_offscreen: true,
                    ..top_level_document()
                },
            ),
        ] {
            let value_read = Cell::new(false);

            let selected = read_candidate_after_provenance(
                || Ok(false),
                || Ok(untrusted.clone()),
                || {
                    value_read.set(true);
                    Ok("https://leaked.example.test/private".to_owned())
                },
            )
            .unwrap();

            assert!(
                selected.is_none(),
                "a node where {description} must not be selected"
            );
            assert!(
                !value_read.get(),
                "the UIA value pattern must not be read when {description}"
            );
        }
    }

    #[test]
    fn focus_loss_after_enumeration_returns_structured_unavailable() {
        let (selected, diagnostics) =
            read_with_focus_script(&[true, false], vec![top_level_document()]);

        assert!(selected.is_none());
        assert_eq!(
            diagnostics,
            vec![BrowserUrlDiagnostic::new(
                "after_enumeration",
                "focus_changed",
            )]
        );
    }

    #[test]
    fn focus_loss_after_candidate_traversal_returns_structured_unavailable() {
        let (selected, diagnostics) =
            read_with_focus_script(&[true, true, false], vec![top_level_document()]);

        assert!(selected.is_none());
        assert_eq!(
            diagnostics,
            vec![BrowserUrlDiagnostic::new(
                "after_candidate_traversal",
                "focus_changed",
            )]
        );
    }

    #[test]
    fn each_absence_reports_its_own_fixed_category() {
        // The categories are the only externally visible signal that the
        // selector has stopped resolving. Collapsing them back to one code
        // would make a Chromium change that withdrew document ValuePattern
        // indistinguishable from a user with two windows side by side.
        let ambiguous = vec![
            top_level_document(),
            UiaElementSnapshot {
                value: Some("https://example.test/second-pane".to_owned()),
                ..top_level_document()
            },
        ];
        let cases = [
            ("no_top_level_document", Vec::new()),
            ("ambiguous_documents", ambiguous),
            (
                "value_unavailable",
                vec![UiaElementSnapshot {
                    value: None,
                    ..top_level_document()
                }],
            ),
            (
                "non_http_scheme",
                vec![UiaElementSnapshot {
                    value: Some("devtools://devtools/bundled/inspector.html".to_owned()),
                    ..top_level_document()
                }],
            ),
        ];

        for (expected_reason, snapshots) in cases {
            let (selected, diagnostics) = read_with_focus_script(&[true, true, true], snapshots);

            assert!(selected.is_none(), "{expected_reason}");
            assert_eq!(
                diagnostics,
                vec![BrowserUrlDiagnostic::new("selection", expected_reason)],
                "{expected_reason}"
            );
        }
    }

    #[test]
    fn uia_failure_reports_only_stable_stage_and_reason() {
        let diagnostics = RefCell::new(Vec::new());
        let selected = read_with_steps(
            "chrome.exe",
            42,
            |_| true,
            || Err(BrowserUrlDiagnostic::uia_unavailable("setup")),
            |_: ()| Ok(()),
            |_| Ok(Vec::new()),
            |diagnostic| diagnostics.borrow_mut().push(diagnostic),
        );

        assert!(selected.is_none());
        assert_eq!(
            diagnostics.into_inner(),
            vec![BrowserUrlDiagnostic::uia_unavailable("setup")]
        );
    }

    #[test]
    fn browser_url_diagnostic_renders_only_fixed_stage_and_reason() {
        assert_eq!(
            BrowserUrlDiagnostic::new("selection", "no_top_level_document").event_line(),
            "event=browser_url_unavailable stage=selection reason=no_top_level_document"
        );
    }

    /// Old omnibox identity, kept only inside the qualification probe so the
    /// probe can report what the retired path would have produced. Production
    /// no longer contains this predicate.
    fn looks_like_retired_omnibox(automation_id: &str, name: &str) -> bool {
        matches!(
            automation_id.to_ascii_lowercase().as_str(),
            "view_1012" | "view_1022"
        ) && matches!(
            name.to_ascii_lowercase().as_str(),
            "address and search bar" | "search or enter web address" | "omnibox"
        )
    }

    fn is_http_url(value: &str) -> bool {
        matches!(Url::parse(value.trim()), Ok(url) if matches!(url.scheme(), "http" | "https"))
    }

    fn elapsed_bucket(elapsed: std::time::Duration) -> &'static str {
        match elapsed.as_millis() {
            0..50 => "lt50ms",
            50..200 => "50-200",
            200..1000 => "200-1000",
            _ => "gt1000",
        }
    }

    /// The qualification gate for the document selector.
    ///
    /// Prints counts and fixed categories only - never a URL, a host, or a
    /// length. Run with a browser foreground on an ordinary `https://` page and
    /// nothing focused in the omnibox, once for `chrome.exe` and once for
    /// `msedge.exe`, plus once with DevTools docked and once with Edge
    /// split-screen to prove the ambiguity path fires.
    #[test]
    #[ignore = "qualification gate: requires a browser foreground on an ordinary https page with the omnibox unfocused"]
    fn browser_url_document_probe() {
        for pass in 1..=2 {
            if pass == 2 {
                std::thread::sleep(std::time::Duration::from_secs(3));
            }
            let started = std::time::Instant::now();
            let handle = foreground_window_handle().expect("foreground window must be available");
            let automation = UIAutomation::new().expect("UIA must be available");
            let root = automation
                .element_from_handle(Handle::from(handle))
                .expect("foreground element must resolve");
            let root_runtime_id = root.get_runtime_id().expect("runtime id must be readable");
            let walker = automation
                .get_control_view_walker()
                .expect("control view walker must be available");
            let condition_for = |control_type: ControlType| {
                automation
                    .create_property_condition(
                        UIProperty::ControlType,
                        Variant::from(control_type as i32),
                        None,
                    )
                    .expect("control type condition must build")
            };

            let documents = root
                .find_all(TreeScope::Subtree, &condition_for(ControlType::Document))
                .unwrap_or_default();
            let documents_total = documents.len();
            let mut documents_top_level = 0;
            let mut documents_rootwebarea_id = 0;
            let mut documents_onscreen = 0;
            let mut doc_value_pattern_ok = 0;
            let mut doc_value_http = 0;
            let mut doc_legacy_value_ok = 0;
            let mut doc_legacy_http = 0;
            for document in &documents {
                if has_document_ancestor(document, &root_runtime_id, &walker).unwrap_or(true) {
                    continue;
                }
                documents_top_level += 1;
                if document.get_automation_id().unwrap_or_default() != ROOT_WEB_AREA_AUTOMATION_ID {
                    continue;
                }
                documents_rootwebarea_id += 1;
                if document.is_offscreen().unwrap_or(true) {
                    continue;
                }
                documents_onscreen += 1;

                if let Ok(value) = document
                    .get_pattern::<UIValuePattern>()
                    .and_then(|pattern| pattern.get_value())
                {
                    doc_value_pattern_ok += 1;
                    if is_http_url(&value) {
                        doc_value_http += 1;
                    }
                }
                if let Ok(value) = document
                    .get_pattern::<UILegacyIAccessiblePattern>()
                    .and_then(|pattern| pattern.get_value())
                {
                    doc_legacy_value_ok += 1;
                    if is_http_url(&value) {
                        doc_legacy_http += 1;
                    }
                }
            }

            // The retired omnibox path, measured beside the replacement. This
            // is what distinguishes "scheme elision" from some other cause: an
            // omnibox value that is not a URL but becomes one once a scheme is
            // prefixed is elision and nothing else.
            let mut omnibox_value_http = 0;
            let mut omnibox_http_after_https_prefix = 0;
            for edit in root
                .find_all(TreeScope::Subtree, &condition_for(ControlType::Edit))
                .unwrap_or_default()
            {
                if has_document_ancestor(&edit, &root_runtime_id, &walker).unwrap_or(true) {
                    continue;
                }
                let automation_id = edit.get_automation_id().unwrap_or_default();
                let name = edit.get_name().unwrap_or_default();
                if !looks_like_retired_omnibox(&automation_id, &name) {
                    continue;
                }
                let Ok(value) = edit
                    .get_pattern::<UIValuePattern>()
                    .and_then(|pattern| pattern.get_value())
                else {
                    continue;
                };
                if is_http_url(&value) {
                    omnibox_value_http += 1;
                } else if is_http_url(&format!("https://{}", value.trim())) {
                    omnibox_http_after_https_prefix += 1;
                }
            }

            let elapsed_bucket = elapsed_bucket(started.elapsed());
            drop(automation);
            println!(
                "BROWSER_URL_DOC_PROBE pass={pass}\n  \
                 documents_total={documents_total}\n  \
                 documents_top_level={documents_top_level}\n  \
                 documents_rootwebarea_id={documents_rootwebarea_id}\n  \
                 documents_onscreen={documents_onscreen}\n  \
                 doc_value_pattern_ok={doc_value_pattern_ok}\n  \
                 doc_value_http={doc_value_http}\n  \
                 doc_legacy_value_ok={doc_legacy_value_ok}   doc_legacy_http={doc_legacy_http}\n  \
                 omnibox_value_http={omnibox_value_http}\n  \
                 omnibox_http_after_https_prefix={omnibox_http_after_https_prefix}\n  \
                 elapsed_bucket={elapsed_bucket}"
            );
        }
    }
}
