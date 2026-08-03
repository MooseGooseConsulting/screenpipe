use std::fmt;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use uiautomation::controls::ControlType;
use uiautomation::core::UICondition;
use uiautomation::patterns::UIValuePattern;
use uiautomation::types::{Handle, TreeScope, UIProperty};
use uiautomation::variants::Variant;
use uiautomation::{UIAutomation, UIElement, UITreeWalker};
use url::Url;

use crate::ForegroundMetadata;
use crate::windows_metadata::foreground_window_handle;

const UIA_EDIT_CONTROL_TYPE_ID: i32 = 50_004;

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

pub fn select_address_bar(app_key: &str, elements: &[UiaElementSnapshot]) -> Option<Url> {
    if !matches!(
        app_key.to_ascii_lowercase().as_str(),
        "chrome.exe" | "msedge.exe"
    ) {
        return None;
    }

    elements.iter().find_map(|element| {
        if element.control_type != UIA_EDIT_CONTROL_TYPE_ID
            || !element.is_enabled
            || element.is_offscreen
            || element.has_document_ancestor
            || !looks_like_address_bar(element)
        {
            return None;
        }

        let url = Url::parse(element.value.as_deref()?.trim()).ok()?;
        matches!(url.scheme(), "http" | "https").then_some(url)
    })
}

fn looks_like_address_bar(element: &UiaElementSnapshot) -> bool {
    element.automation_id.eq_ignore_ascii_case("view_1022")
        && matches!(
            element.name.to_ascii_lowercase().as_str(),
            "address and search bar" | "search or enter web address" | "omnibox"
        )
}

pub struct BrowserUrlReader;

impl BrowserUrlReader {
    pub fn read_for_foreground(&self, metadata: &ForegroundMetadata) -> Result<Option<Url>> {
        if !matches!(
            metadata.app_key.to_ascii_lowercase().as_str(),
            "chrome.exe" | "msedge.exe"
        ) {
            return Ok(None);
        }

        Ok(read_with_steps(
            &metadata.app_key,
            metadata.window_handle,
            is_still_foreground,
            || setup_uia_traversal(metadata.window_handle),
            enumerate_edit_controls,
            read_chrome_candidates,
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
    let edit_condition = automation
        .create_property_condition(
            UIProperty::ControlType,
            Variant::from(ControlType::Edit as i32),
            None,
        )
        .map_err(|_| unavailable())?;
    let walker = automation
        .get_control_view_walker()
        .map_err(|_| unavailable())?;

    Ok((automation, root, edit_condition, walker, root_runtime_id))
}

fn enumerate_edit_controls(setup: UiaSetup) -> Result<UiaEnumeration, BrowserUrlDiagnostic> {
    let (automation, root, edit_condition, walker, root_runtime_id) = setup;
    let edits = root
        .find_all(TreeScope::Subtree, &edit_condition)
        .map_err(|_| BrowserUrlDiagnostic::uia_unavailable("enumeration"))?;
    Ok((automation, root, walker, root_runtime_id, edits))
}

fn read_chrome_candidates(
    enumeration: UiaEnumeration,
) -> Result<Vec<UiaElementSnapshot>, BrowserUrlDiagnostic> {
    let (_automation, _root, walker, root_runtime_id, edits) = enumeration;
    let mut snapshots = Vec::new();
    for element in edits {
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

fn read_candidate_value(element: &UIElement) -> Result<String, BrowserUrlDiagnostic> {
    let unavailable = || BrowserUrlDiagnostic::uia_unavailable("candidate_traversal");
    element
        .get_pattern::<UIValuePattern>()
        .and_then(|pattern| pattern.get_value())
        .map_err(|_| unavailable())
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

    let mut snapshot = read_identity()?;
    if snapshot.control_type != UIA_EDIT_CONTROL_TYPE_ID
        || !snapshot.is_enabled
        || snapshot.is_offscreen
        || !looks_like_address_bar(&snapshot)
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

    let selected = select_address_bar(app_key, &snapshots);
    if selected.is_none() {
        diagnose(BrowserUrlDiagnostic::new(
            "selection",
            "no_trusted_address_bar",
        ));
    }
    selected
}

fn emit_diagnostic(diagnostic: BrowserUrlDiagnostic) {
    tracing::debug!(
        browser_url_stage = diagnostic.stage,
        browser_url_reason = diagnostic.reason,
        "browser URL metadata unavailable"
    );
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};

    use super::*;

    fn trusted_address_bar() -> UiaElementSnapshot {
        UiaElementSnapshot {
            control_type: UIA_EDIT_CONTROL_TYPE_ID,
            name: "Address and search bar".to_owned(),
            automation_id: "view_1022".to_owned(),
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
    fn exact_collision_under_document_reads_no_dom_identity_or_value() {
        let identity_read = Cell::new(false);
        let value_read = Cell::new(false);

        let selected = read_candidate_after_provenance(
            || Ok(true),
            || {
                identity_read.set(true);
                Ok(UiaElementSnapshot {
                    has_document_ancestor: true,
                    ..trusted_address_bar()
                })
            },
            || {
                value_read.set(true);
                Ok("https://page.example.test/private".to_owned())
            },
        )
        .unwrap();

        assert!(selected.is_none());
        assert!(!identity_read.get());
        assert!(!value_read.get());
    }

    #[test]
    fn focus_loss_after_enumeration_returns_structured_unavailable() {
        let (selected, diagnostics) =
            read_with_focus_script(&[true, false], vec![trusted_address_bar()]);

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
            read_with_focus_script(&[true, true, false], vec![trusted_address_bar()]);

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
    fn no_trusted_candidate_returns_structured_unavailable() {
        let (selected, diagnostics) = read_with_focus_script(&[true, true, true], Vec::new());

        assert!(selected.is_none());
        assert_eq!(
            diagnostics,
            vec![BrowserUrlDiagnostic::new(
                "selection",
                "no_trusted_address_bar",
            )]
        );
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
}
