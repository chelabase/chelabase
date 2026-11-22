//! Role checks shared by rest, storage and the realtime service: whether a role may back a
//! secret key at all, and which of the data API's two logins a session as it runs on.
//!
//! # Invariant: one login per role
//!
//! The data API has two logins. `authenticator` holds the ordinary key and console roles and
//! its own `anon` and `authenticated`; [`ELEVATED_LOGIN`] holds the elevated ones (the roles
//! only an Admin may key or act as) and nothing else. Postgres checks every role switch
//! against the session's login, so what a session can become is what its login holds.
//!
//! 1. **The login is chosen from the catalog, by [`login_for`] alone.** A role session's
//!    login is a function of the role's name and its [`RoleFacts`] read by
//!    [`OWNER_LEVEL_SQL`]: nothing a request carries chooses it.
//! 2. **An elevated role never runs on `authenticator`.** A role whose facts say `elevated`
//!    runs on the elevated login when that login holds it directly, and nowhere otherwise
//!    ([`login_for`] answers `None`: a 503). The exception is the data API's own `anon` and
//!    `authenticated`, which stay on `authenticator` whatever they were made members of.
//! 3. **An ordinary role never runs on the elevated login.** The elevated login may reach a
//!    role it doesn't hold (an elevated role it holds is a member of it), so a role switch
//!    there proves nothing. A connection of the elevated login is therefore handed to a
//!    session only after the facts were read again on that connection and still choose it
//!    (`chelabase_pgsession::owner_level::still_on_elevated`; realtime reads them inside the
//!    check's transaction). Cached facts alone never run a role there.
//! 4. **A stale answer fails closed.** Facts may be cached. A session opened on
//!    `authenticator` when it no longer reaches the role fails in its preamble (the role
//!    switch is refused); on the elevated login the read of rule 3 refuses it. Either way
//!    the caller drops the cached facts and answers 503. It is never tried on the other
//!    login without facts read anew.
//!
//!
//! Limits, both bounded by the cache's lifetime (`chelabase_pgsession::owner_level`, 30 s):
//!
//! - Rule 2 is about the facts a request used. A role an Admin makes elevated keeps running on
//!   `authenticator` from facts cached before the change, until the entry expires or the
//!   worker moves the grant (the preamble then fails: rule 4). `authenticator` holds the role
//!   for that long whatever the cache says; moving a grant the moment a role changes is the
//!   worker's to do.
//! - After a move, the first request with older cached facts answers 503 once.
//!
//! Rule 3 has no such window in rest and storage: the elevated login's facts are read for
//! every session. What is left is a role change that commits between that read and the
//! session's transaction, which is a change made while the request runs.
//!
//! `tests::routing_holds_the_invariant` checks rules 2 and 3 over every combination of facts;
//! `chelabase_pgsession::owner_level`'s model test runs every short sequence of role changes,
//! grant moves, cache expiries and requests against rules 2 to 4 and these limits, with and
//! without the elevated login reaching the role through another role.

/// The data API's second login (the worker's `data_api::AUTHENTICATOR_ELEVATED`).
pub const ELEVATED_LOGIN: &str = "chelabase_authenticator_elevated";

/// About role `$1`, in one row (no row: the role doesn't exist):
///
/// 0. Whether it may not back a secret key (E33), exactly what the worker's create-time check
///    refuses: superuser, an attribute that administers (CREATEROLE, CREATEDB, REPLICATION), or
///    membership at any depth in a superuser role or one of
///    [`chelabase_types::data_key::KEY_ROLE_REFUSED_GROUPS`] (`neon_superuser` and the system
///    roles). BYPASSRLS alone isn't refused.
/// 1. Whether it is elevated, the worker's `member_roles::is_elevated`: it, or a role it is a
///    member of through any membership, can log in, has an owner-level attribute (BYPASSRLS
///    included), holds ADMIN on some role, or has a name no member may be granted
///    (`chelabase_types::access::act_as_reserved`, and the owner `postgres`). Names are
///    lowered by Postgres here, so a non-ASCII name may count as reserved where the worker's
///    ASCII rule doesn't; that only refuses.
/// 2. Whether [`ELEVATED_LOGIN`] holds it directly (`pg_auth_members`, by OID).
pub const OWNER_LEVEL_SQL: &str = "SELECT r.rolsuper OR r.rolcreaterole OR r.rolcreatedb OR r.rolreplication \
    OR EXISTS (SELECT FROM pg_catalog.pg_roles s WHERE (s.rolsuper OR s.rolname IN ('neon_superuser', \
    'chelabase_admin', 'pg_execute_server_program', 'pg_read_server_files', 'pg_write_server_files')) \
    AND pg_catalog.pg_has_role(r.oid, s.oid, 'MEMBER')), \
    EXISTS (SELECT FROM pg_catalog.pg_roles x, pg_catalog.lower(x.rolname::pg_catalog.text) AS l \
    WHERE pg_catalog.pg_has_role(r.oid, x.oid, 'MEMBER') \
    AND (x.rolsuper OR x.rolcreaterole OR x.rolcreatedb OR x.rolreplication OR x.rolbypassrls OR x.rolcanlogin \
    OR EXISTS (SELECT FROM pg_catalog.pg_auth_members a WHERE a.admin_option AND a.member OPERATOR(pg_catalog.=) x.oid) \
    OR (x.rolname::pg_catalog.text OPERATOR(pg_catalog.<>) ALL (ARRAY['anon', 'authenticated']) \
    AND (pg_catalog.starts_with(l, 'pg_') OR pg_catalog.starts_with(l, 'chelabase_') OR pg_catalog.starts_with(l, 'cb_m_') \
    OR l OPERATOR(pg_catalog.=) ANY (ARRAY['owner', 'postgres', 'anon', 'authenticated', 'authenticator', \
    'chelabase_admin', 'cloud_admin', 'neon_superuser', 'public', 'none', 'chelabase_auth_sync']))))), \
    EXISTS (SELECT FROM pg_catalog.pg_auth_members m JOIN pg_catalog.pg_roles e ON e.oid OPERATOR(pg_catalog.=) m.member \
    WHERE m.roleid OPERATOR(pg_catalog.=) r.oid AND e.rolname OPERATOR(pg_catalog.=) 'chelabase_authenticator_elevated') \
    FROM pg_catalog.pg_roles r WHERE r.rolname = $1";

/// What [`OWNER_LEVEL_SQL`] answers about a role that exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RoleFacts {
    /// It may not back a secret key or a console session.
    pub owner_level: bool,
    /// Only an Admin may key or act as it.
    pub elevated: bool,
    /// [`ELEVATED_LOGIN`] holds it directly.
    pub held_by_elevated: bool,
}

impl RoleFacts {
    /// The row [`OWNER_LEVEL_SQL`] answered. A NULL anywhere (never, from that statement)
    /// reads as the refusing answer: owner-level, elevated, not held.
    pub fn from_row(row: &tokio_postgres::Row) -> Result<Self, tokio_postgres::Error> {
        Ok(RoleFacts {
            owner_level: row.try_get::<_, Option<bool>>(0)?.unwrap_or(true),
            elevated: row.try_get::<_, Option<bool>>(1)?.unwrap_or(true),
            held_by_elevated: row.try_get::<_, Option<bool>>(2)?.unwrap_or(false),
        })
    }
}

/// One of the data API's two logins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Login {
    /// `authenticator` (for a service's managed pool: its own login).
    Authenticator,
    /// [`ELEVATED_LOGIN`].
    Elevated,
}

/// The login a session as `role` runs on, from the role's `facts` (`None`: it doesn't
/// exist), or `None` when it may run on neither: an elevated role the elevated login doesn't
/// hold (yet). See the module's invariant.
///
/// - `anon` and `authenticated` are always `authenticator`'s.
/// - A role that doesn't exist goes to `authenticator`, where the preamble reports it.
pub fn login_for(role: &str, facts: Option<&RoleFacts>) -> Option<Login> {
    if chelabase_types::access::data_api_role(role) {
        return Some(Login::Authenticator);
    }
    match facts {
        None => Some(Login::Authenticator),
        Some(f) if !f.elevated => Some(Login::Authenticator),
        Some(f) if f.held_by_elevated => Some(Login::Elevated),
        Some(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_facts() -> Vec<Option<RoleFacts>> {
        let mut all = vec![None];
        for bits in 0..8u8 {
            all.push(Some(RoleFacts {
                owner_level: bits & 1 != 0,
                elevated: bits & 2 != 0,
                held_by_elevated: bits & 4 != 0,
            }));
        }
        all
    }

    /// Every combination of facts, for a key role and for the data API's own roles: an
    /// elevated role never gets `authenticator`, an ordinary one never the elevated login,
    /// and the elevated login is chosen only when it holds the role.
    #[test]
    fn routing_holds_the_invariant() {
        for facts in all_facts() {
            let login = login_for("reporter", facts.as_ref());
            match facts {
                None => assert_eq!(login, Some(Login::Authenticator)),
                Some(f) => {
                    if f.elevated {
                        assert_ne!(login, Some(Login::Authenticator), "{f:?}");
                        assert_eq!(login == Some(Login::Elevated), f.held_by_elevated, "{f:?}");
                    } else {
                        assert_eq!(login, Some(Login::Authenticator), "{f:?}");
                    }
                }
            }
            for own in ["anon", "authenticated"] {
                assert_eq!(login_for(own, facts.as_ref()), Some(Login::Authenticator), "{own} {facts:?}");
            }
        }
        // Only the exact names are the data API's own.
        let elevated = RoleFacts { owner_level: false, elevated: true, held_by_elevated: false };
        assert_eq!(login_for("Anon", Some(&elevated)), None);
    }
}
