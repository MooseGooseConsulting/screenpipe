use std::fmt;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use uiautomation::controls::ControlType;
use uiautomation::patterns::UIValuePattern;
use uiautomation::types::{Handle, TreeScope, UIProperty};
use uiautomation::variants::Variant;
use uiautomation::{UIAutomation, UIElement};
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

        if !is_still_foreground(metadata.window_handle) {
            return Ok(None);
        }

        let automation = match UIAutomation::new() {
            Ok(automation) => automation,
            Err(_) => return Ok(None),
        };
        let root = match automation.element_from_handle(Handle::from(metadata.window_handle)) {
            Ok(root) => root,
            Err(_) => return Ok(None),
        };
        let edit_condition = match automation.create_property_condition(
            UIProperty::ControlType,
            Variant::from(ControlType::Edit as i32),
            None,
        ) {
            Ok(condition) => condition,
            Err(_) => return Ok(None),
        };
        let edits = match root.find_all(TreeScope::Subtree, &edit_condition) {
            Ok(edits) => edits,
            Err(_) => return Ok(None),
        };
        if !is_still_foreground(metadata.window_handle) {
            return Ok(None);
        }

        let snapshots = edits
            .iter()
            .filter_map(snapshot_address_bar_candidate)
            .collect::<Vec<_>>();
        if !is_still_foreground(metadata.window_handle) {
            return Ok(None);
        }

        Ok(select_address_bar(&metadata.app_key, &snapshots))
    }
}

fn is_still_foreground(expected_handle: isize) -> bool {
    foreground_window_handle().ok() == Some(expected_handle)
}

fn snapshot_address_bar_candidate(element: &UIElement) -> Option<UiaElementSnapshot> {
    let mut snapshot = UiaElementSnapshot {
        control_type: element.get_control_type().ok()? as i32,
        name: element.get_name().ok()?,
        automation_id: element.get_automation_id().ok()?,
        value: None,
        is_enabled: element.is_enabled().ok()?,
        is_offscreen: element.is_offscreen().ok()?,
    };
    if !looks_like_address_bar(&snapshot) {
        return None;
    }

    snapshot.value = element
        .get_pattern::<UIValuePattern>()
        .and_then(|pattern| pattern.get_value())
        .ok();
    Some(snapshot)
}
