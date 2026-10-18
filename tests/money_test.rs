use xecut_bot::money::parse_euros;

#[test]
fn euro_amounts_are_exact_cents_and_reject_ambiguous_input() {
    assert_eq!(parse_euros("50").unwrap(), 5000);
    assert_eq!(parse_euros("0.01").unwrap(), 1);
    assert_eq!(parse_euros("12.30").unwrap(), 1230);
    for invalid in [
        "0",
        "-1",
        "NaN",
        "1e3",
        "0.001",
        "1.234",
        "9223372036854775808",
    ] {
        assert!(parse_euros(invalid).is_err(), "accepted {invalid}");
    }
}
