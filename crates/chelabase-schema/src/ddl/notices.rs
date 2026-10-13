//! Postgres's own account of what a `DROP … CASCADE` removed: its `drop cascades to …` notice.

/// What every line (and the single-object message) of the notice starts with.
const PREFIX: &str = "drop cascades to ";
/// The tail of the detail's last line when Postgres listed only the first objects.
const TRUNCATED: &str = " other objects (see server log for list)";

/// The object descriptions (`pg_describe_object`'s text) one notice reports a cascading drop
/// removed: `Some(descriptions)` for a `drop cascades to …` notice, `Some(vec![])` for any other
/// notice, `None` when the notice can't be read as a complete list (truncated, a count that
/// disagrees with the lines listed, malformed).
///
/// Postgres reports one object in the message (`drop cascades to view v`), several as
/// `drop cascades to N other objects` with one `drop cascades to <description>` line per object
/// in the detail, and at most 100 lines, then `and M other objects (see server log for list)`.
pub fn cascaded_objects(message: &str, detail: Option<&str>) -> Option<Vec<String>> {
    let Some(rest) = message.strip_prefix(PREFIX.trim_end()) else {
        return Some(vec![]);
    };
    let rest = rest.strip_prefix(' ').filter(|r| !r.is_empty())?;
    let Some(count) = other_objects(rest) else {
        return Some(vec![rest.to_string()]);
    };
    // Lines are split at each `\ndrop cascades to `, so a description holding a newline
    // stays whole (or, at worst, miscounts and reads as unknown).
    let detail = detail?.strip_prefix(PREFIX)?;
    let objects: Vec<String> = detail
        .split(&format!("\n{PREFIX}"))
        .map(str::to_string)
        .collect();
    let truncated = objects
        .last()
        .is_some_and(|last| last.contains("\nand ") && last.ends_with(TRUNCATED));
    if truncated || objects.len() != count || objects.iter().any(String::is_empty) {
        return None;
    }
    Some(objects)
}

/// `N` of `N other object(s)`.
fn other_objects(rest: &str) -> Option<usize> {
    let (n, tail) = rest.split_once(' ')?;
    if !matches!(tail, "other objects" | "other object") || !n.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    n.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_description_with_a_newline_stays_whole() {
        let detail = "drop cascades to view \"a\nb\"\ndrop cascades to view c";
        assert_eq!(
            cascaded_objects("drop cascades to 2 other objects", Some(detail)),
            Some(vec!["view \"a\nb\"".to_string(), "view c".to_string()])
        );
    }

    #[test]
    fn a_count_without_its_detail_is_unknown() {
        assert_eq!(
            cascaded_objects("drop cascades to 2 other objects", None),
            None
        );
        assert_eq!(
            cascaded_objects("drop cascades to 2 other objects", Some("something else")),
            None
        );
        assert_eq!(
            cascaded_objects(
                "drop cascades to 2 other objects",
                Some("drop cascades to a\ndrop cascades to ")
            ),
            None
        );
    }

    #[test]
    fn a_name_that_looks_like_a_count_is_one_object() {
        assert_eq!(
            cascaded_objects("drop cascades to view x other objects", None),
            Some(vec!["view x other objects".to_string()])
        );
    }

    #[test]
    fn a_prefix_inside_a_word_is_another_notice() {
        assert_eq!(cascaded_objects("drop cascades tomorrow", None), None);
        assert_eq!(cascaded_objects("dropped", None), Some(vec![]));
    }
}
