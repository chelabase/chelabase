-- Edge functions m8 (audit round 3): a function's run-as binding on a branch remembers the role
-- OID a member approved (`set_run_as`), and a new branch copies it, so the worker mints a
-- child's own key only for that very role (branches share role OIDs from the branch point),
-- never for a namesake. `run_as_unmintable` records why the worker never will ('missing': no
-- role by that name on the branch; 'changed': a role by that name with another OID, or no
-- approved OID to compare; 'refused': the role checks refuse it) until a member sets run-as
-- again. Existing bindings take the OID of the key they hold.
ALTER TABLE branch_functions
    ADD COLUMN run_as_role_oid bigint,
    ADD COLUMN run_as_unmintable text CHECK (run_as_unmintable IN ('missing', 'changed', 'refused'));
UPDATE branch_functions bf SET run_as_role_oid = k.role_oid
FROM data_api_keys k
WHERE k.id = bf.run_as_key_id AND k.branch_id = bf.branch_id;
ALTER TABLE branch_functions ADD CONSTRAINT branch_functions_run_as_identity CHECK (
    run_as_role IS NOT NULL OR (run_as_role_oid IS NULL AND run_as_unmintable IS NULL)
);
