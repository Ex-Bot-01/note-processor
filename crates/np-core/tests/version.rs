#[test]
fn version_is_semver() {
    let parts: Vec<&str> = np_core::M_VERSION.split('.').collect();
    assert_eq!(parts.len(), 3, "expected MAJOR.MINOR.PATCH");
    assert!(parts.iter().all(|p| p.chars().all(|c| c.is_ascii_digit())));
}
