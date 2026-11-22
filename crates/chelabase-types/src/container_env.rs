//! Names a member service container's environment may not carry.
//!
//! Invariant: a name is reserved exactly when it is `PORT` (the runner's own), starts with
//! `LD_` (the dynamic loader's variables), or is one of [`LOADER_NAMES`] (a runtime or libc
//! reads it at start to load code, modules or options). Matching is byte-exact and
//! case-sensitive, as Linux and the loaders read the environment: `ld_preload` is not
//! reserved. The orchestrator refuses such a secret on a service at write time, and the
//! service runner refuses to build an environment that holds one, both through
//! [`reserved_container_env_name`], so the two can never disagree.

/// The port the container listens on, which only the runner sets.
pub const PORT_ENV: &str = "PORT";

/// Every name starting with this is the dynamic loader's (`LD_PRELOAD`, `LD_LIBRARY_PATH`,
/// `LD_AUDIT`, …).
pub const LOADER_PREFIX: &str = "LD_";

/// Exact names a runtime, shell or libc reads at start to load code, modules or options.
pub const LOADER_NAMES: &[&str] = &[
    // Node.js
    "NODE_OPTIONS",
    "NODE_PATH",
    // Python
    "PYTHONSTARTUP",
    "PYTHONPATH",
    "PYTHONHOME",
    "PYTHONUSERBASE",
    "PYTHONWARNINGS",
    // JVM
    "JAVA_TOOL_OPTIONS",
    "_JAVA_OPTIONS",
    "JDK_JAVA_OPTIONS",
    // Perl
    "PERL5OPT",
    "PERL5LIB",
    "PERLLIB",
    // Ruby
    "RUBYOPT",
    "RUBYLIB",
    // Shells
    "BASH_ENV",
    "ENV",
    "SHELLOPTS",
    "BASHOPTS",
    "PS4",
    // glibc
    "GCONV_PATH",
    "GLIBC_TUNABLES",
    // .NET
    "DOTNET_STARTUP_HOOKS",
    "DOTNET_ADDITIONAL_DEPS",
    "CORECLR_ENABLE_PROFILING",
    "CORECLR_PROFILER_PATH",
    "CORECLR_PROFILER_PATH_32",
    "CORECLR_PROFILER_PATH_64",
];

/// Whether a container environment may not carry `name` (see the module's invariant).
pub fn reserved_container_env_name(name: &str) -> bool {
    name == PORT_ENV || name.starts_with(LOADER_PREFIX) || LOADER_NAMES.contains(&name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The model: the invariant written out independently of the implementation.
    fn model(name: &str) -> bool {
        let b = name.as_bytes();
        name == "PORT"
            || (b.len() >= 3 && &b[..3] == b"LD_")
            || LOADER_NAMES.iter().any(|n| n.as_bytes() == b)
    }

    #[test]
    fn every_listed_name_and_every_ld_name_is_reserved() {
        for name in [
            "PORT",
            "LD_PRELOAD",
            "LD_LIBRARY_PATH",
            "LD_AUDIT",
            "LD_",
            "LD_ANYTHING",
            "NODE_OPTIONS",
            "NODE_PATH",
            "PYTHONSTARTUP",
            "PYTHONPATH",
            "PYTHONHOME",
            "JAVA_TOOL_OPTIONS",
            "_JAVA_OPTIONS",
            "JDK_JAVA_OPTIONS",
            "PERL5OPT",
            "PERL5LIB",
            "PERLLIB",
            "RUBYOPT",
            "RUBYLIB",
            "BASH_ENV",
            "ENV",
            "GCONV_PATH",
            "GLIBC_TUNABLES",
            "DOTNET_STARTUP_HOOKS",
            "PYTHONWARNINGS",
            "PYTHONUSERBASE",
            "SHELLOPTS",
            "BASHOPTS",
            "PS4",
            "CORECLR_ENABLE_PROFILING",
            "CORECLR_PROFILER_PATH",
            "CORECLR_PROFILER_PATH_32",
            "CORECLR_PROFILER_PATH_64",
            "DOTNET_ADDITIONAL_DEPS",
        ] {
            assert!(reserved_container_env_name(name), "{name}");
        }
    }

    #[test]
    fn near_misses_are_not_reserved() {
        for name in [
            "ld_preload",
            "Ld_PRELOAD",
            "LDX",
            "LD",
            "XLD_PRELOAD",
            "NODE_OPTION",
            "NODE_OPTIONS_",
            "node_options",
            "ENVIRONMENT",
            "env",
            "PORTS",
            "port",
            "PYTHONPATH2",
            "JAVA_OPTIONS",
            "PS4X",
            "PS1",
            "shellopts",
            "CORECLR_PROFILER",
            "CORECLR_PROFILER_PATH_128",
            "PYTHONWARNING",
            "CLASSPATH",
            "PATH",
            "API_KEY",
            "",
        ] {
            assert!(!reserved_container_env_name(name), "{name}");
        }
    }

    /// Exhaustive over every name of up to three bytes from an alphabet that holds each
    /// boundary (case, `_`, the `LD_` prefix and the short exact names), plus every listed
    /// name with one byte added, removed or case-flipped: the predicate equals the model.
    #[test]
    fn the_predicate_matches_the_model_exhaustively() {
        let alphabet: Vec<char> = "LDdlNVEe_PORTXS4".chars().collect();
        let mut names = vec![String::new()];
        let mut frontier = vec![String::new()];
        for _ in 0..3 {
            let mut next = Vec::new();
            for prefix in &frontier {
                for c in &alphabet {
                    next.push(format!("{prefix}{c}"));
                }
            }
            names.extend(next.iter().cloned());
            frontier = next;
        }
        for listed in LOADER_NAMES.iter().copied().chain(["PORT", "LD_PRELOAD"]) {
            names.push(listed.to_string());
            for i in 0..listed.len() {
                let mut removed = listed.to_string();
                removed.remove(i);
                names.push(removed);
                let mut flipped: Vec<u8> = listed.bytes().collect();
                flipped[i] = if flipped[i].is_ascii_uppercase() {
                    flipped[i].to_ascii_lowercase()
                } else {
                    flipped[i].to_ascii_uppercase()
                };
                names.push(String::from_utf8(flipped).unwrap());
            }
            for c in &alphabet {
                names.push(format!("{listed}{c}"));
                names.push(format!("{c}{listed}"));
            }
        }
        for name in &names {
            assert_eq!(reserved_container_env_name(name), model(name), "{name:?}");
        }
    }

    /// The model's own assumption: the list holds no duplicates and no `LD_` name (the
    /// prefix already covers those), and every entry is a valid environment name.
    #[test]
    fn the_list_is_well_formed() {
        let mut seen = std::collections::HashSet::new();
        for name in LOADER_NAMES {
            assert!(seen.insert(name), "duplicate {name}");
            assert!(
                !name.starts_with(LOADER_PREFIX),
                "{name} is covered by the prefix"
            );
            assert!(
                name.starts_with(|c: char| c.is_ascii_uppercase() || c == '_'),
                "{name}"
            );
            assert!(
                name.bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_'),
                "{name}"
            );
        }
    }
}
