# Rewrites a rendered compose config (`docker compose config --format json`) so each service's
# file secrets appear as the read-only bind mounts Docker makes of them (`docker inspect` shows
# them as binds, RW false), appended to the service's `volumes` and dropped from `secrets`. The
# tests that reason about what a service can see parse `volumes`; they apply this first.
# A secret without a `target` lands at /run/secrets/<name>, as Compose does.
. as $r
| .services |= with_entries(.value |= (
    ((.secrets // []) | map(
        (if type == "string" then {source: .} else . end)
        | {type: "bind", source: $r.secrets[.source].file,
           target: (.target // ("/run/secrets/" + .source)), read_only: true,
           bind: {create_host_path: false}})) as $s
    | if ($s | length) == 0 then del(.secrets)
      else .volumes = ((.volumes // []) + $s) | del(.secrets) end))
