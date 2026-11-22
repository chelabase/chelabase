//! The Postgres types sqlx declares for the jiff backing's binds: the same as chrono's, so
//! `$n::timestamptz IS NULL` and `unnest($n::timestamptz[])` with NULLs keep their types.

use chelabase_time::jiff_backed::{Date, Timestamp};
use sqlx::postgres::types::Oid;
use sqlx::postgres::{PgHasArrayType, PgTypeInfo, Postgres};
use sqlx::{Type, TypeInfo};

fn oid(t: PgTypeInfo) -> Option<u32> {
    t.oid().map(|Oid(o)| o)
}

#[test]
fn type_info_is_timestamptz_date_and_arrays() {
    type C = chrono::DateTime<chrono::Utc>;
    type CD = chrono::NaiveDate;
    let cases = [
        (
            <Timestamp as Type<Postgres>>::type_info(),
            <C as Type<Postgres>>::type_info(),
            1184,
        ),
        (
            <Option<Timestamp> as Type<Postgres>>::type_info(),
            <Option<C> as Type<Postgres>>::type_info(),
            1184,
        ),
        (
            <Vec<Timestamp> as Type<Postgres>>::type_info(),
            <Vec<C> as Type<Postgres>>::type_info(),
            1185,
        ),
        (
            <Vec<Option<Timestamp>> as Type<Postgres>>::type_info(),
            <Vec<Option<C>> as Type<Postgres>>::type_info(),
            1185,
        ),
        (
            <&[Timestamp] as Type<Postgres>>::type_info(),
            <&[C] as Type<Postgres>>::type_info(),
            1185,
        ),
        (
            <Timestamp as PgHasArrayType>::array_type_info(),
            <C as PgHasArrayType>::array_type_info(),
            1185,
        ),
        (
            <Date as Type<Postgres>>::type_info(),
            <CD as Type<Postgres>>::type_info(),
            1082,
        ),
        (
            <Option<Date> as Type<Postgres>>::type_info(),
            <Option<CD> as Type<Postgres>>::type_info(),
            1082,
        ),
        (
            <Vec<Date> as Type<Postgres>>::type_info(),
            <Vec<CD> as Type<Postgres>>::type_info(),
            1182,
        ),
    ];
    for (jiff, chrono, expected) in cases {
        assert_eq!(oid(jiff.clone()), Some(expected), "{jiff:?}");
        assert_eq!(oid(chrono.clone()), Some(expected), "{chrono:?}");
        assert_eq!(jiff, chrono);
        assert!(jiff.type_eq(&chrono));
        // A column of the type is compatible with the jiff type, as with chrono's.
        assert!(chrono.type_compatible(&jiff));
    }
    assert!(<Timestamp as Type<Postgres>>::compatible(&<C as Type<
        Postgres,
    >>::type_info(
    )));
    assert!(<Vec<Timestamp> as Type<Postgres>>::compatible(
        &<Vec<C> as Type<Postgres>>::type_info()
    ));
    assert!(<Date as Type<Postgres>>::compatible(&<CD as Type<
        Postgres,
    >>::type_info()));
    // `timestamp` (without time zone) is a different type: a bind of it would shift instants.
    assert!(!<Timestamp as Type<Postgres>>::compatible(
        &<chrono::NaiveDateTime as Type<Postgres>>::type_info()
    ));
}
