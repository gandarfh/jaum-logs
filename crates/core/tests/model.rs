use jaum_core::{Constraint, Enforce};

fn c(text: &str, pattern: Option<&str>) -> Constraint {
    Constraint {
        text: text.to_string(),
        enforce: Enforce::Hook,
        pattern: pattern.map(String::from),
    }
}

#[test]
fn hook_pattern_uses_explicit_pattern_when_present() {
    let c = c("do not touch src/legacy/", Some("src/(legacy|old)/"));
    assert_eq!(c.hook_pattern(), "src/(legacy|old)/");
}

#[test]
fn hook_pattern_derives_path_from_text() {
    let c = c("do not touch src/legacy/", None);
    // path extracted, with `.` escaped if it had one
    assert_eq!(c.hook_pattern(), "src/legacy/");
}

#[test]
fn hook_pattern_escapes_dot_in_path() {
    let c = c("do not edit config.toml", None);
    assert_eq!(c.hook_pattern(), r"config\.toml");
}

#[test]
fn hook_pattern_derives_keyword_when_no_path() {
    let c = c("do not run migration", None);
    // stopwords (not, run) dropped
    assert_eq!(c.hook_pattern(), "migration");
}

#[test]
fn hook_pattern_alternates_multiple_significant_words() {
    let c = c("do not use reflection runtime", None);
    let p = c.hook_pattern();
    assert!(p.contains("reflection"));
    assert!(p.contains("runtime"));
    assert!(p.contains('|'));
}
