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
fn identity_full_case_folds_sharp_s_before_hashing() {
    let mixed_case = TextIdentity::from_ocr("Straße");
    let uppercase = TextIdentity::from_ocr("STRASSE");

    assert_eq!(mixed_case.normalized, "strasse");
    assert_eq!(uppercase.normalized, "strasse");
    assert_eq!(
        mixed_case.exact_hash,
        "16d96952087774fee069b7585d3991b24d90c181c09b2129b4908c35baa7f0c0"
    );
    assert_eq!(uppercase.exact_hash, mixed_case.exact_hash);
}

#[test]
fn identity_full_case_folds_capital_sharp_s_before_hashing() {
    let identity = TextIdentity::from_ocr("ẞ");

    assert_eq!(identity.normalized, "ss");
    assert_eq!(
        identity.exact_hash,
        "a31fe9656fc8d3a459e623dc8204e6d0268f8df56d734dac3ca3262edb5db883"
    );
}

#[test]
fn identity_full_case_folds_greek_sigma_variants_before_hashing() {
    let final_sigma = TextIdentity::from_ocr("ς");
    let capital_sigma = TextIdentity::from_ocr("Σ");

    assert_eq!(final_sigma.normalized, "σ");
    assert_eq!(capital_sigma.normalized, "σ");
    assert_eq!(
        final_sigma.exact_hash,
        "ff59ed1e562b056aea9bfdf757bada8c1eb638b10b75bfc7835beca4c2db0360"
    );
    assert_eq!(capital_sigma.exact_hash, final_sigma.exact_hash);
}

#[test]
fn identity_recomposes_case_fold_output_before_hashing() {
    let identity = TextIdentity::from_ocr("ǰ");

    assert_eq!(identity.normalized, "ǰ");
    assert_eq!(
        identity.exact_hash,
        "9ec0c487e469e80e7bfacc64b889adc03dc9b2213890aa147b3dd4c7b6536b16"
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
