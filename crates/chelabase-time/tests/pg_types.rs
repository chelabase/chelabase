//! The Postgres types sqlx declares for the binds: the OIDs chrono's codecs declared, so
//! `$n::timestamptz IS NULL` and `unnest($n::timestamptz[])` with NULLs keep their types.

use chelabase_time::{Date, Timestamp};
use sqlx::postgres::types::Oid;
use sqlx::postgres::{PgHasArrayType, PgTypeInfo, Postgres};
use sqlx::{Type, TypeInfo};

fn oid(t: PgTypeInfo) -> Option<u32> {
    t.oid().map(|Oid(o)| o)
}

#[test]
fn type_info_is_timestamptz_date_and_arrays() {
    let cases = [
        (<Timestamp as Type<Postgres>>::type_info(), 1184),
        (<Option<Timestamp> as Type<Postgres>>::type_info(), 1184),
        (<Vec<Timestamp> as Type<Postgres>>::type_info(), 1185),
        (
            <Vec<Option<Timestamp>> as Type<Postgres>>::type_info(),
            1185,
        ),
        (<&[Timestamp] as Type<Postgres>>::type_info(), 1185),
        (<Timestamp as PgHasArrayType>::array_type_info(), 1185),
        (<Date as Type<Postgres>>::type_info(), 1082),
        (<Option<Date> as Type<Postgres>>::type_info(), 1082),
        (<Vec<Date> as Type<Postgres>>::type_info(), 1182),
    ];
    for (info, expected) in cases {
        assert_eq!(oid(info.clone()), Some(expected), "{info:?}");
        let builtin = PgTypeInfo::with_oid(Oid(expected));
        assert_eq!(info, builtin);
        assert!(info.type_eq(&builtin));
        assert!(builtin.type_compatible(&info));
    }
    // A column of the type is compatible with the bind type.
    assert!(<Timestamp as Type<Postgres>>::compatible(
        &PgTypeInfo::with_oid(Oid(1184))
    ));
    assert!(<Vec<Timestamp> as Type<Postgres>>::compatible(
        &PgTypeInfo::with_oid(Oid(1185))
    ));
    assert!(<Date as Type<Postgres>>::compatible(&PgTypeInfo::with_oid(
        Oid(1082)
    )));
    // `timestamp` (without time zone, OID 1114) is a different type: a bind of it would shift
    // instants.
    assert!(!<Timestamp as Type<Postgres>>::compatible(
        &PgTypeInfo::with_oid(Oid(1114))
    ));
}
