# Draft: report to Neon, event-trigger privilege escalation (not yet sent)

**Not sent: decided by the user on 2026-10-03 (upstream Neon is frozen and has no disclosure process). The fix is in the `chelabase/neon` fork, landed before its 2026-10-04 history replay (with a second gap closed: the check no longer depends on `neon.event_triggers`).**

**Status:** draft for the user to send. Private; keep it out of the public fork repos until the embargo ends (see the fork plan's Status block).
**Send to:** `security@neon.tech`, or through Neon's HackerOne program (`https://hackerone.com/neon_bbp`), per `https://neon.com/security`. GitHub private vulnerability reporting is disabled on `neondatabase/neon`.
**Found by:** the step 6 (data API) session, 2026-10-01. Confirmed on `neondatabase/neon` `main` at `fa504217c61bbcaf5c512d75830564541f917f8f`.
**Sent:** _(date and channel go here once sent; the 14-day embargo counts from that date)_

---

**Subject:** Privilege escalation to superuser through event triggers and trusted extensions in pgxn/neon (neon_ddl_handler.c)

Hello Neon security team,

We maintain a fork of neondatabase/neon and found a privilege escalation in the `neon` Postgres extension. It lets a database owner who is a `neon_superuser` member but not a superuser become a real superuser (`rolsuper = true`) on their compute.

**Affected code:** `pgxn/neon/neon_ddl_handler.c`, `neon_fmgr_hook`, the `FHET_START` branch (around lines 964–1010 at `fa504217`). The hook turns event-trigger functions owned by non-superusers into no-ops only when `superuser_arg(GetSessionUserId())` is true. It never checks the current user (`GetUserId()`).

**Reproduction** (Postgres 17 compute; no non-Neon event triggers present):
1. As the owner role (a `neon_superuser` member, not a superuser), create a plpgsql function owned by the owner. When `current_user <> session_user`, it runs `ALTER ROLE <owner> SUPERUSER`. Then `CREATE EVENT TRIGGER ... ON ddl_command_end EXECUTE FUNCTION` it. The create succeeds through `switch_to_superuser`, and `alter_event_trigger_owner` gives the trigger back to the owner.
2. As the owner, run `CREATE EXTENSION pgcrypto` (a trusted extension). Its install script runs as the bootstrap superuser while the session user is still the owner. The owner's event trigger fires with a superuser `current_user`, and because the session user isn't a superuser, the hook doesn't neutralise the function.
3. `SELECT rolsuper FROM pg_roles WHERE rolname = '<owner>'` now returns `t`.

**Impact:** any database owner on a Neon compute can gain superuser and, from there, run arbitrary code as the Postgres OS user on the compute. Other tenants and storage are affected only as far as a compute superuser can reach.

**Suggested fix:** also neutralise (or refuse) a non-superuser-owned event-trigger function when `superuser_arg(GetUserId())` is true. Add a regression test where an owner event trigger exists, `CREATE EXTENSION` installs a trusted extension, and the owner stays non-superuser.

**Disclosure:** we're fixing this in our fork with a neutral commit message now. We plan to publish a description 14 days after this report, unless you'd like a different timeline; reply and we'll coordinate.

Regards,
<name>
