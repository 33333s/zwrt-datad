//! Normalized, bounded LAN configuration for authenticated App clients.
use crate::model::Snapshot;
use serde::Deserialize;
use serde_json::{Value, json};
use std::net::Ipv4Addr;

fn ipv4(value: &str) -> Option<u32> {
    value.parse::<Ipv4Addr>().ok().map(u32::from)
}
fn mask(value: &str) -> Option<u32> {
    let m = ipv4(value)?;
    let host = !m;
    ((1..=30).contains(&m.leading_ones()) && host & host.wrapping_add(1) == 0).then_some(m)
}
fn lease_seconds(raw: &str) -> Option<u64> {
    let (n, scale) = match raw.chars().last()? {
        'h' => (&raw[..raw.len() - 1], 3600),
        'm' => (&raw[..raw.len() - 1], 60),
        's' => (&raw[..raw.len() - 1], 1),
        'd' => (&raw[..raw.len() - 1], 86400),
        _ => (raw, 1),
    };
    n.parse::<u64>()
        .ok()?
        .checked_mul(scale)
        .filter(|n| *n > 0 && *n <= 2_592_000)
}
pub fn settings(snapshot: &Snapshot, fixed_address: bool) -> Value {
    let dhcp = snapshot.fields.get("dhcp").unwrap_or(&Value::Null);
    let string = |key: &str| {
        dhcp.get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    };
    let ip = string("ip").filter(|s| ipv4(s).is_some());
    let netmask = string("netmask").filter(|s| mask(s).is_some());
    let mtu = dhcp
        .get("mtu")
        .and_then(Value::as_u64)
        .or_else(|| {
            snapshot
                .fields
                .get("uci_device_info")
                .and_then(|v| v.get("mtu"))
                .and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok()))
        })
        .filter(|m| (576..=1500).contains(m));
    json!({"supported":ip.is_some() && netmask.is_some(), "ip":ip,"netmask":netmask,
        "address_writable":!fixed_address,"dhcp_enabled":dhcp.get("disabled").and_then(Value::as_bool).map(|b| !b),
        "dhcp_start":string("range_start"),"dhcp_end":string("range_end"),
        "lease_seconds":string("leasetime").and_then(lease_seconds),"mtu":mtu,
        "mtu_min":576,"mtu_max":1500,"lease_step_seconds":3600,"lease_max_seconds":2592000})
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Update {
    pub ip: Option<String>,
    pub netmask: Option<String>,
    pub dhcp_enabled: Option<bool>,
    pub dhcp_start: Option<String>,
    pub dhcp_end: Option<String>,
    pub lease_seconds: Option<u64>,
}
impl Update {
    pub fn params(&self, current: &Value) -> Result<Value, &'static str> {
        if current["supported"] != true {
            return Err("lan_unavailable");
        }
        let read = |key: &str| current[key].as_str().unwrap_or_default();
        let ip = self.ip.as_deref().unwrap_or_else(|| read("ip"));
        let netmask = self.netmask.as_deref().unwrap_or_else(|| read("netmask"));
        if current["address_writable"] != true && (ip != read("ip") || netmask != read("netmask")) {
            return Err("lan_address_readonly");
        }
        let gateway = ipv4(ip).ok_or("invalid_lan_ip")?;
        let m = mask(netmask).ok_or("invalid_lan_netmask")?;
        let network = gateway & m;
        let broadcast = network | !m;
        let first = gateway >> 24;
        if first == 0 || first == 127 || first >= 224 || gateway == network || gateway == broadcast
        {
            return Err("invalid_lan_ip");
        }
        let enabled = self
            .dhcp_enabled
            .or_else(|| current["dhcp_enabled"].as_bool())
            .ok_or("lan_unavailable")?;
        let mut params = json!({"ip":ip,"netmask":netmask,"dhcp_disabled":if enabled {0} else {1}});
        if enabled {
            let start = self
                .dhcp_start
                .as_deref()
                .unwrap_or_else(|| read("dhcp_start"));
            let end = self.dhcp_end.as_deref().unwrap_or_else(|| read("dhcp_end"));
            let a = ipv4(start).ok_or("invalid_dhcp_range")?;
            let b = ipv4(end).ok_or("invalid_dhcp_range")?;
            if a > b
                || a & m != network
                || b & m != network
                || a <= network
                || b >= broadcast
                || (a..=b).contains(&gateway)
            {
                return Err("invalid_dhcp_range");
            }
            let lease = self
                .lease_seconds
                .or_else(|| current["lease_seconds"].as_u64())
                .ok_or("invalid_dhcp_lease")?;
            if !(3600..=2592000).contains(&lease) || lease % 3600 != 0 {
                return Err("invalid_dhcp_lease");
            }
            params["dhcp_start"] = json!(start);
            params["dhcp_end"] = json!(end);
            // Generic ZWRT router_set_lan_para takes a decimal string; U50 accepts either.
            params["lease_seconds"] = json!(lease.to_string());
        }
        Ok(params)
    }
}

pub fn matches(params: &Value, current: &Value) -> bool {
    params.as_object().is_some_and(|p| {
        p.iter().all(|(key, value)| match key.as_str() {
            "dhcp_disabled" => current["dhcp_enabled"].as_bool() == value.as_i64().map(|n| n == 0),
            "lease_seconds" | "mtu" => {
                current[key].as_u64() == value.as_str().and_then(|s| s.parse().ok())
            }
            _ => &current[key] == value,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn current() -> Value {
        json!({"supported":true,"address_writable":false,"ip":"192.168.0.1",
        "netmask":"255.255.255.0","dhcp_enabled":true,"dhcp_start":"192.168.0.2","dhcp_end":"192.168.0.253","lease_seconds":86400})
    }
    #[test]
    fn rejects_network_broadcast_gateway_and_other_subnets() {
        for (a, b) in [
            ("192.168.0.0", "192.168.0.20"),
            ("192.168.0.2", "192.168.0.255"),
            ("192.168.0.1", "192.168.0.20"),
            ("192.168.1.2", "192.168.1.20"),
            ("192.168.0.20", "192.168.0.2"),
        ] {
            assert!(
                Update {
                    dhcp_start: Some(a.into()),
                    dhcp_end: Some(b.into()),
                    ..Default::default()
                }
                .params(&current())
                .is_err()
            );
        }
        assert!(
            Update {
                ip: Some("192.168.1.1".into()),
                ..Default::default()
            }
            .params(&current())
            .is_err()
        );
        assert_eq!(
            Update {
                lease_seconds: Some(30),
                ..Default::default()
            }
            .params(&current()),
            Err("invalid_dhcp_lease")
        );
        assert!(matches(
            &Update::default().params(&current()).unwrap(),
            &current()
        ));
    }
    #[test]
    fn explicit_units_unknown_reads_and_editable_gateway() {
        assert_eq!(lease_seconds("12h"), Some(43200));
        assert_eq!(lease_seconds("86400"), Some(86400));
        assert_eq!(lease_seconds("30m"), Some(1800));
        assert_eq!(lease_seconds("forever"), None);
        assert!(mask("255.0.255.0").is_none());
        let mut c = current();
        c["address_writable"] = json!(true);
        assert!(
            Update {
                ip: Some("192.168.1.1".into()),
                dhcp_start: Some("192.168.1.2".into()),
                dhcp_end: Some("192.168.1.200".into()),
                ..Default::default()
            }
            .params(&c)
            .is_ok()
        );
        assert!(
            Update {
                dhcp_enabled: Some(false),
                ..Default::default()
            }
            .params(&c)
            .unwrap()
            .get("dhcp_start")
            .is_none()
        );
    }
}
