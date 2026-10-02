//! Last-resort WAN addresses straight from the kernel: the addresses of the
//! interface that carries the default route. Used only when neither netifd
//! nor the vendor data service reports one (G5 Pro: the data call bypasses
//! netifd and `zwrt_data` can drift to "disconnected" while traffic flows).
//! Reads /proc and `getifaddrs`; no external command is started.
use serde_json::{Value, json};
use std::net::{Ipv4Addr, Ipv6Addr};

const RTF_UP: u32 = 0x0001;
const RTF_REJECT: u32 = 0x0200;
const IFA_F_DADFAILED: u32 = 0x08;
const IFA_F_TENTATIVE: u32 = 0x40;

/// LAN-side and local interfaces never count as the uplink.
fn uplink_candidate(name: &str) -> bool {
    !name.is_empty() && name != "lo" && !name.starts_with("br-")
}

/// Interface of the IPv4 default route with the lowest metric (`/proc/net/route`).
fn default_ipv4_interface(route: &str) -> Option<String> {
    route
        .lines()
        .skip(1)
        .filter_map(|line| {
            let columns: Vec<&str> = line.split_whitespace().collect();
            let flags = u32::from_str_radix(columns.get(3)?, 16).ok()?;
            let metric: u32 = columns.get(6)?.parse().ok()?;
            (columns.get(1)? == &"00000000"
                && columns.get(7)? == &"00000000"
                && flags & RTF_UP != 0
                && flags & RTF_REJECT == 0
                && uplink_candidate(columns[0]))
            .then(|| (metric, columns[0].to_owned()))
        })
        .min()
        .map(|(_, name)| name)
}

/// Interface of the IPv6 default route (`/proc/net/ipv6_route`); unreachable
/// placeholders on `lo` (RTF_REJECT, metric ffffffff) are ignored.
fn default_ipv6_interface(route: &str) -> Option<String> {
    route
        .lines()
        .filter_map(|line| {
            let columns: Vec<&str> = line.split_whitespace().collect();
            if columns.len() < 10 || columns[0].len() != 32 || columns[1] != "00" {
                return None;
            }
            if columns[0].bytes().any(|byte| byte != b'0') {
                return None;
            }
            let metric = u32::from_str_radix(columns[5], 16).ok()?;
            let flags = u32::from_str_radix(columns[8], 16).ok()?;
            (flags & RTF_UP != 0 && flags & RTF_REJECT == 0 && uplink_candidate(columns[9]))
                .then(|| (metric, columns[9].to_owned()))
        })
        .min()
        .map(|(_, name)| name)
}

fn usable_ipv4(address: Ipv4Addr) -> bool {
    !(address.is_unspecified()
        || address.is_loopback()
        || address.is_link_local()
        || address.is_broadcast()
        || address.is_multicast())
}

fn usable_ipv6(address: Ipv6Addr) -> bool {
    !(address.is_unspecified()
        || address.is_loopback()
        || address.is_multicast()
        || address.is_unicast_link_local())
}

/// Global, settled addresses of `interface` from `/proc/net/if_inet6`.
fn ipv6_addresses(if_inet6: &str, interface: &str) -> Vec<Value> {
    if_inet6
        .lines()
        .filter_map(|line| {
            let columns: Vec<&str> = line.split_whitespace().collect();
            if columns.len() < 6 || columns[5] != interface || columns[0].len() != 32 {
                return None;
            }
            let address = Ipv6Addr::from(u128::from_str_radix(columns[0], 16).ok()?);
            let prefix = u8::from_str_radix(columns[2], 16).ok()?;
            let scope = u32::from_str_radix(columns[3], 16).ok()?;
            let flags = u32::from_str_radix(columns[4], 16).ok()?;
            (scope == 0 && flags & (IFA_F_TENTATIVE | IFA_F_DADFAILED) == 0 && usable_ipv6(address))
                .then(|| json!({"address":address.to_string(),"mask":prefix}))
        })
        .take(4)
        .collect()
}

/// IPv4 addresses of `interface` from `getifaddrs` (netlink inside libc).
fn ipv4_addresses(interface: &str) -> Vec<Value> {
    let mut out = Vec::new();
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `head` with a list we only read and then free.
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        return out;
    }
    let mut cursor = head;
    while !cursor.is_null() && out.len() < 4 {
        // SAFETY: `cursor` walks the list returned above until its null end.
        let entry = unsafe { &*cursor };
        cursor = entry.ifa_next;
        if entry.ifa_addr.is_null() || entry.ifa_name.is_null() {
            continue;
        }
        // SAFETY: non-null C string owned by the list.
        let name = unsafe { std::ffi::CStr::from_ptr(entry.ifa_name) };
        // SAFETY: ifa_addr is non-null; the family is checked before the cast.
        if name.to_bytes() != interface.as_bytes()
            || i32::from(unsafe { (*entry.ifa_addr).sa_family }) != libc::AF_INET
        {
            continue;
        }
        // SAFETY: AF_INET addresses are sockaddr_in.
        let address = unsafe { &*(entry.ifa_addr as *const libc::sockaddr_in) };
        let address = Ipv4Addr::from(u32::from_be(address.sin_addr.s_addr));
        if !usable_ipv4(address) {
            continue;
        }
        let mut item = json!({"address":address.to_string()});
        if !entry.ifa_netmask.is_null() {
            // SAFETY: the netmask of an AF_INET entry is a sockaddr_in.
            let mask = unsafe { &*(entry.ifa_netmask as *const libc::sockaddr_in) };
            item["mask"] = json!(u32::from_be(mask.sin_addr.s_addr).count_ones());
        }
        out.push(item);
    }
    // SAFETY: freeing the list obtained from getifaddrs exactly once.
    unsafe { libc::freeifaddrs(head) };
    out
}

fn proc_root() -> String {
    std::env::var("ZWRT_DATAD_PROC_ROOT").unwrap_or_else(|_| "/proc".into())
}

/// IPv4 addresses of the IPv4 default-route interface.
pub fn ipv4() -> Vec<Value> {
    std::fs::read_to_string(format!("{}/net/route", proc_root()))
        .ok()
        .and_then(|route| default_ipv4_interface(&route))
        .map(|interface| ipv4_addresses(&interface))
        .unwrap_or_default()
}

/// Global IPv6 addresses of the IPv6 default-route interface.
pub fn ipv6() -> Vec<Value> {
    let root = proc_root();
    let Some(interface) = std::fs::read_to_string(format!("{root}/net/ipv6_route"))
        .ok()
        .and_then(|route| default_ipv6_interface(&route))
    else {
        return Vec::new();
    };
    std::fs::read_to_string(format!("{root}/net/if_inet6"))
        .map(|text| ipv6_addresses(&text, &interface))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Captured on a G5 Pro (MC8532B) whose data call bypasses netifd.
    const ROUTE: &str =
        "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT
rmnet_data0\t00000000\t7AF9CB79\t0003\t0\t0\t0\t00000000\t0\t0\t0
rmnet_data0\t78F9CB79\t00000000\t0001\t0\t0\t0\tFCFFFFFF\t0\t0\t0
eth0\t0016FEA9\t00000000\t0001\t0\t0\t0\t00FFFFFF\t0\t0\t0
br-lan\t0000A8C0\t00000000\t0001\t0\t0\t128\t00FFFFFF\t0\t0\t0
";
    const IPV6_ROUTE: &str = "00000000000000000000000000000000 00 00000000000000000000000000000000 00 00000000000000000000000000000000 ffffffff 00000001 00000000 00200200       lo
";

    #[test]
    fn ipv4_default_route_interface_is_the_uplink() {
        assert_eq!(
            default_ipv4_interface(ROUTE).as_deref(),
            Some("rmnet_data0")
        );
        // LAN bridges, loopback, rejected or down routes never qualify.
        let lan_only = "Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask
br-lan\t00000000\t0100A8C0\t0003\t0\t0\t0\t00000000
lo\t00000000\t00000000\t0001\t0\t0\t0\t00000000
wwan0\t00000000\t00000000\t0201\t0\t0\t0\t00000000
usb0\t00000000\t00000000\t0002\t0\t0\t0\t00000000
";
        assert_eq!(default_ipv4_interface(lan_only), None);
        let two = "Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask
rmnet_data1\t00000000\t01010101\t0003\t0\t0\t20\t00000000
rmnet_data0\t00000000\t01010101\t0003\t0\t0\t10\t00000000
";
        assert_eq!(default_ipv4_interface(two).as_deref(), Some("rmnet_data0"));
    }

    #[test]
    fn ipv6_unreachable_placeholders_are_not_a_default_route() {
        assert_eq!(default_ipv6_interface(IPV6_ROUTE), None);
        let real = "00000000000000000000000000000000 00 00000000000000000000000000000000 00 fe800000000000000000000000000001 00000400 00000001 00000000 00450003 rmnet_data0\n";
        assert_eq!(default_ipv6_interface(real).as_deref(), Some("rmnet_data0"));
    }

    #[test]
    fn only_global_settled_ipv6_addresses_are_used() {
        let if_inet6 = "fe800000000000005e4dbffffe9764a7 22 40 20 80 rmnet_data0
24080000000000000000000000000001 22 40 00 00 rmnet_data0
24080000000000000000000000000002 22 40 00 40 rmnet_data0
24080000000000000000000000000003 23 40 00 00 br-lan
00000000000000000000000000000001 01 80 10 80 lo
";
        assert_eq!(
            ipv6_addresses(if_inet6, "rmnet_data0"),
            vec![json!({"address":"2408::1","mask":64})]
        );
    }

    #[test]
    fn local_ipv4_ranges_are_rejected() {
        for bad in [
            "169.254.22.1",
            "127.0.0.1",
            "0.0.0.0",
            "255.255.255.255",
            "224.0.0.1",
        ] {
            assert!(!usable_ipv4(bad.parse().unwrap()), "{bad}");
        }
        // Carrier-grade NAT and private carrier ranges are real uplink addresses.
        for good in ["121.203.249.121", "100.64.1.2", "10.20.30.40"] {
            assert!(usable_ipv4(good.parse().unwrap()), "{good}");
        }
    }

    #[test]
    fn loopback_interface_reads_through_getifaddrs() {
        // `lo` is excluded as an uplink, but the reader itself must work and
        // drop loopback addresses.
        assert!(ipv4_addresses("lo").is_empty());
        assert!(ipv4_addresses("no-such-interface").is_empty());
    }
}
