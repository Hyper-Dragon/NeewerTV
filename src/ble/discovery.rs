//! Choosing which advertised device is ours.
//!
//! The name is derived from the *model*, not the unit — every 5 m strip
//! advertises `NH-PD20250145` — so matching it exactly would only ever work for
//! the model the author happened to own. A pattern is the minimum needed to find
//! the 3 m variant without a rebuild.
//!
//! This is a deliberately small glob: `*` is the only metacharacter, and it
//! matches any run of characters. That is enough for every real case
//! (`NH-PD*`, `*20250145`) and avoids a dependency for something a user types
//! once.

/// Does `name` match `pattern`?
///
/// `*` matches any sequence, including an empty one. Matching is case-sensitive,
/// because a device name is an exact string on the wire and accepting a
/// near-miss would make a typo look like it worked.
#[must_use]
pub fn matches(pattern: &str, name: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();

    // No wildcard: the pattern is the whole name.
    if parts.len() == 1 {
        return pattern == name;
    }

    let first = parts[0];
    let last = parts[parts.len() - 1];

    if !name.starts_with(first) || !name.ends_with(last) {
        return false;
    }

    // A pattern like `*` or `**` has no infixes to check.
    if parts.len() == 2 {
        return true;
    }

    // Walk the middle segments in order through the region between the
    // matched prefix and suffix. Anything out of order is a non-match.
    let mut cursor = first.len();
    let end = name.len() - last.len();
    for part in &parts[1..parts.len() - 1] {
        if part.is_empty() {
            continue;
        }
        let Some(found) = name[cursor..end].find(part) else {
            return false;
        };
        cursor += found + part.len();
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_exact_pattern_matches_only_itself() {
        assert!(matches("NH-PD20250145", "NH-PD20250145"));
        assert!(!matches("NH-PD20250145", "NH-PD20250103"));
        assert!(!matches("NH-PD20250145", "NH-PD20250145X"));
    }

    #[test]
    fn a_trailing_wildcard_covers_every_model_of_the_family() {
        // The default, and the reason it exists: this must find the 5 m and the
        // 3 m strip without being told which one.
        assert!(matches("NH-PD*", "NH-PD20250145"));
        assert!(matches("NH-PD*", "NH-PD20250103"));
        assert!(!matches("NH-PD*", "NH-XX20250145"));
    }

    #[test]
    fn a_leading_wildcard_matches_a_suffix() {
        assert!(matches("*20250145", "NH-PD20250145"));
        assert!(!matches("*20250145", "NH-PD20250103"));
    }

    #[test]
    fn a_wildcard_in_the_middle_matches_both_ends() {
        assert!(matches("NH-*145", "NH-PD20250145"));
        assert!(!matches("NH-*145", "NH-PD20250103"));
    }

    #[test]
    fn a_bare_wildcard_matches_everything() {
        assert!(matches("*", ""));
        assert!(matches("*", "anything at all"));
    }

    #[test]
    fn a_wildcard_matches_an_empty_run() {
        assert!(matches("NH-PD*", "NH-PD"));
        assert!(matches("*NH-PD*", "NH-PD"));
    }

    #[test]
    fn several_wildcards_are_checked_in_order() {
        assert!(matches("*PD*145", "NH-PD20250145"));
        assert!(!matches("*PD*145", "NH-PD20250103"));
        // Order matters: the suffix must come after the infix.
        assert!(!matches("*145*PD", "NH-PD20250145"));
    }

    #[test]
    fn matching_is_case_sensitive() {
        assert!(!matches("nh-pd*", "NH-PD20250145"));
    }
}
