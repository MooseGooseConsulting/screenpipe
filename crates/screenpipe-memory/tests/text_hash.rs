use screenpipe_memory::{TextIdentity, jaccard_overlap};

#[test]
fn identity_normalizes_nfkc_case_and_whitespace_before_hashing() {
    let identity = TextIdentity::from_ocr("  ＧＯＡＬ\tOne\r\n");

    assert_eq!(identity.normalized, "goal one");
    assert_eq!(
        identity.exact_hash,
        "6d7cd9298a03854d9e8a65d727f839b5919adb122ee00086ff94a4ce73282cb3"
    );
}

#[test]
fn identity_uses_literal_word_five_grams() {
    let identity = TextIdentity::from_ocr("one two three four five six");

    assert_eq!(
        identity.five_grams.into_iter().collect::<Vec<_>>(),
        vec!["one two three four five", "two three four five six"]
    );
}

#[test]
fn empty_ocr_has_a_hash_but_no_five_grams() {
    let identity = TextIdentity::from_ocr(" \t\r\n ");

    assert_eq!(identity.normalized, "");
    assert_eq!(
        identity.exact_hash,
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert!(identity.five_grams.is_empty());
}

#[test]
fn overlap_is_jaccard_intersection_over_union() {
    let left = TextIdentity::from_ocr("one two three four five six seven eight nine ten");
    let right = TextIdentity::from_ocr("three four five six seven eight nine ten eleven twelve");

    assert_eq!(jaccard_overlap(&left.five_grams, &right.five_grams), 0.5);
}

#[test]
fn empty_gram_sets_have_no_overlap() {
    let left = TextIdentity::from_ocr("");
    let right = TextIdentity::from_ocr("");

    assert_eq!(jaccard_overlap(&left.five_grams, &right.five_grams), 0.0);
}
