//! Added network latency via `tc netem` in the compute container's namespace.
//! Everything returns argv vectors for `std::process::Command` (no shell).

pub const NETSHOOT: &str = "nicolaka/netshoot:v0.16@sha256:b09d9b21381f47a79b3cbcb30da25266dc17186ea00ae65e99fdc51396f48e70";

fn tc(container: &str, tail: &[&str]) -> Vec<String> {
    let mut v: Vec<String> = ["docker", "run", "--rm", "--net"].map(String::from).into();
    v.push(format!("container:{container}"));
    v.extend(["--cap-add", "NET_ADMIN", NETSHOOT, "tc"].map(String::from));
    v.extend(tail.iter().map(|s| s.to_string()));
    v
}

/// Commands, in order, that delay traffic to `dst` by `rtt_us` (everything else is
/// untouched, via a prio qdisc whose default band is not delayed). Empty for 0.
pub fn add_cmds(container: &str, rtt_us: u32, dst: std::net::IpAddr) -> Vec<Vec<String>> {
    if rtt_us == 0 {
        return Vec::new();
    }
    let delay = format!("{rtt_us}us");
    let (proto, m, d) = match dst {
        std::net::IpAddr::V4(ip) => ("ip", "ip", format!("{ip}/32")),
        std::net::IpAddr::V6(ip) => ("ipv6", "ip6", format!("{ip}/128")),
    };
    vec![
        tc(
            container,
            &[
                "qdisc", "add", "dev", "eth0", "root", "handle", "1:", "prio", "bands", "3",
                "priomap", "1", "1", "1", "1", "1", "1", "1", "1", "1", "1", "1", "1", "1", "1",
                "1", "1",
            ],
        ),
        tc(
            container,
            &[
                "qdisc", "add", "dev", "eth0", "parent", "1:3", "handle", "30:", "netem", "delay",
                &delay,
            ],
        ),
        tc(
            container,
            &[
                "filter", "add", "dev", "eth0", "parent", "1:0", "protocol", proto, "prio", "1",
                "u32", "match", m, "dst", &d, "flowid", "1:3",
            ],
        ),
    ]
}

pub fn del_cmd(container: &str) -> Vec<String> {
    tc(container, &["qdisc", "del", "dev", "eth0", "root"])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(container: &str, tail: &str) -> Vec<String> {
        let mut v: Vec<String> = ["docker", "run", "--rm", "--net"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        v.push(format!("container:{container}"));
        v.extend(["--cap-add", "NET_ADMIN", NETSHOOT, "tc"].map(String::from));
        v.extend(tail.split(' ').map(String::from));
        v
    }

    #[test]
    fn netem_add_cmds_ipv4() {
        let got = add_cmds("c1", 500, "172.17.0.1".parse().unwrap());
        assert_eq!(
            got,
            vec![
                argv(
                    "c1",
                    "qdisc add dev eth0 root handle 1: prio bands 3 priomap 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1"
                ),
                argv(
                    "c1",
                    "qdisc add dev eth0 parent 1:3 handle 30: netem delay 500us"
                ),
                argv(
                    "c1",
                    "filter add dev eth0 parent 1:0 protocol ip prio 1 u32 match ip dst 172.17.0.1/32 flowid 1:3"
                ),
            ]
        );
    }

    #[test]
    fn netem_add_cmds_ipv6() {
        let got = add_cmds("c1", 500, "fd00::1".parse().unwrap());
        assert_eq!(
            got[2],
            argv(
                "c1",
                "filter add dev eth0 parent 1:0 protocol ipv6 prio 1 u32 match ip6 dst fd00::1/128 flowid 1:3"
            )
        );
    }

    #[test]
    fn netem_zero_rtt_adds_nothing() {
        assert!(add_cmds("c1", 0, "172.17.0.1".parse().unwrap()).is_empty());
    }

    #[test]
    fn netem_del_cmd_removes_root() {
        assert_eq!(del_cmd("c1"), argv("c1", "qdisc del dev eth0 root"));
    }
}
