//! `CHELABASE_TIME_BACKING=jiff` compiles the crate with the jiff backing behind the public
//! `Timestamp` and `Date` names, so the goldens run against it unchanged:
//! `CHELABASE_TIME_BACKING=jiff cargo nextest run -p chelabase-time`. Only this crate's own
//! tests build that way; the rest of the workspace still needs the chrono backing.

fn main() {
    println!("cargo::rustc-check-cfg=cfg(chelabase_time_jiff)");
    println!("cargo::rerun-if-env-changed=CHELABASE_TIME_BACKING");
    match std::env::var("CHELABASE_TIME_BACKING").as_deref() {
        Ok("jiff") => println!("cargo::rustc-cfg=chelabase_time_jiff"),
        Ok("chrono") | Err(_) => {}
        Ok(other) => panic!("CHELABASE_TIME_BACKING={other}: expected jiff or chrono"),
    }
}
