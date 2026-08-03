use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use uiautomation::controls::ControlType;
use uiautomation::patterns::UIValuePattern;
use uiautomation::types::{Handle, TreeScope, UIProperty};
use uiautomation::variants::Variant;
use uiautomation::{UIAutomation, UIElement};
use url::Url;

use crate::ForegroundMetadata;

const UIA_EDIT_CONTROL_TYPE_ID: i32 = 50_004;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiaElementSnapshot {
    pub control_type: i32,
    pub name: String,
    pub automation_id: String,
    pub value: Option<String>,
    pub is_enabled: bool,
    pub is_offscreen: bool,
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
    let name = element.name.to_ascii_lowercase();
    let automation_id = element.automation_id.to_ascii_lowercase();
    name.contains("address")
        || name.contains("omnibox")
        || automation_id.contains("address")
        || automation_id.contains("omnibox")
        || automation_id == "view_1022"
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

        let automation = UIAutomation::new().context("create Windows UI Automation client")?;
        let root = automation
            .element_from_handle(Handle::from(metadata.window_handle))
            .context("open foreground browser UI Automation root")?;
        let edit_condition = automation
            .create_property_condition(
                UIProperty::ControlType,
                Variant::from(ControlType::Edit as i32),
                None,
            )
            .context("create browser Edit-control condition")?;
        let edits = root
            .find_all(TreeScope::Subtree, &edit_condition)
            .context("enumerate foreground browser Edit controls")?;
        let snapshots = edits
            .iter()
            .filter_map(snapshot_address_bar_candidate)
            .collect::<Vec<_>>();
        Ok(select_address_bar(&metadata.app_key, &snapshots))
    }
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
