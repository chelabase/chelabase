//! Coordinator ruling R4 (T3 review, Important): after a cascading drop runs, the objects
//! Postgres reports in its `drop cascades to …` notice must be exactly the confirmed
//! dependents' descriptions. `cascaded_objects` reads that notice.
use chelabase_schema::ddl::cascaded_objects;

#[test]
fn one_object_is_in_the_message() {
    assert_eq!(cascaded_objects("drop cascades to view notes_v", None), Some(vec!["view notes_v".to_string()]));
}

#[test]
fn several_objects_are_in_the_detail() {
    let detail = "drop cascades to view notes_v\ndrop cascades to constraint memos_note_fk on table memos\ndrop cascades to default value for column x of table t";
    assert_eq!(
        cascaded_objects("drop cascades to 3 other objects", Some(detail)),
        Some(vec![
            "view notes_v".to_string(),
            "constraint memos_note_fk on table memos".to_string(),
            "default value for column x of table t".to_string(),
        ])
    );
}

#[test]
fn a_truncated_list_is_unknown() {
    // Postgres lists at most 100 objects in the detail, then "and N other objects (see server
    // log for list)": the set can't be compared, so the caller treats it as a mismatch.
    let mut detail: Vec<String> = (0..100).map(|i| format!("drop cascades to view v{i}")).collect();
    detail.push("and 5 other objects (see server log for list)".into());
    assert_eq!(cascaded_objects("drop cascades to 105 other objects", Some(&detail.join("\n"))), None);
    // A count that disagrees with the lines listed is unknown too.
    assert_eq!(cascaded_objects("drop cascades to 3 other objects", Some("drop cascades to view a")), None);
}

#[test]
fn other_notices_are_not_cascades() {
    assert_eq!(cascaded_objects("table \"x\" does not exist, skipping", None), Some(vec![]));
    assert_eq!(cascaded_objects("drop cascades to", None), None, "malformed");
}
