use std::collections::BTreeSet;

use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextIdentity {
    pub normalized: String,
    pub exact_hash: String,
    pub five_grams: BTreeSet<String>,
}

impl TextIdentity {
    pub fn from_ocr(text: &str) -> Self {
        let normalized = normalize_text(text);
        let exact_hash = Sha256::digest(normalized.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let words = normalized.split_whitespace().collect::<Vec<_>>();
        let five_grams = words.windows(5).map(|window| window.join(" ")).collect();

        Self {
            normalized,
            exact_hash,
            five_grams,
        }
    }
}

pub fn jaccard_overlap(left: &BTreeSet<String>, right: &BTreeSet<String>) -> f64 {
    if left.is_empty() && right.is_empty() {
        return 0.0;
    }

    let intersection = left.intersection(right).count() as f64;
    let union = left.union(right).count() as f64;
    intersection / union
}

pub(crate) fn normalize_text(text: &str) -> String {
    text.nfkc()
        .flat_map(char::to_lowercase)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}
