//! APN_PROC_EX as used by the MU5120 WebUI (service.js M/C/F/k).
//! Secrets are used only to preserve omitted fields; never serialize the catalog.
use serde_json::{Map, Value, json};

pub struct Catalog {
    pub raw: Value,
    records: Vec<Option<Vec<String>>>,
    v6: Vec<Option<Vec<String>>>,
}

fn text(value: &str, limit: usize) -> Result<String, String> {
    if value.len() > limit
        || value.chars().any(char::is_control)
        || value.contains("($)")
        || value.contains("||")
    {
        return Err("invalid APN field".into());
    }
    Ok(value.to_owned())
}

fn record(raw: &Value, key: &str) -> Result<Option<Vec<String>>, String> {
    let s = raw[key].as_str().ok_or("incomplete OEM APN catalog")?;
    if s.is_empty() {
        return Ok(None);
    }
    if s.len() > 8192 || s.chars().any(char::is_control) {
        return Err("invalid OEM APN record".into());
    }
    let fields: Vec<String> = s.split("($)").map(str::to_owned).collect();
    if fields.len() != 13 {
        return Err("unsupported OEM APN record layout".into());
    }
    Ok(Some(fields))
}

impl Catalog {
    pub fn parse(raw: Value) -> Result<Self, String> {
        if raw["apn_interface_version"] != "2"
            || !matches!(raw["apn_mode"].as_str(), Some("auto" | "manual"))
        {
            return Err("APN writes require the verified version 2 OEM interface".into());
        }
        let records = (0..20)
            .map(|i| record(&raw, &format!("APN_config{i}")))
            .collect::<Result<Vec<_>, _>>()?;
        let v6 = (0..20)
            .map(|i| record(&raw, &format!("ipv6_APN_config{i}")))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { raw, records, v6 })
    }

    fn at(&self, index: usize) -> Result<&Vec<String>, String> {
        self.records[index]
            .as_ref()
            .filter(|r| !r[7].is_empty())
            .or_else(|| self.v6[index].as_ref().filter(|r| !r[7].is_empty()))
            .ok_or_else(|| "manual APN profile not found".into())
    }

    fn index(params: &Value) -> Result<usize, String> {
        let id = params["profile_id"]
            .as_str()
            .ok_or("profile_id is required")?;
        let n = id
            .strip_prefix("manual-")
            .ok_or("only manual profiles can be changed")?;
        let index = n.parse::<usize>().map_err(|_| "invalid profile_id")?;
        if index >= 20 || n != index.to_string() {
            return Err("invalid profile_id".into());
        }
        Ok(index)
    }

    pub fn plan(&self, action: &str, params: &Value) -> Result<Plan, String> {
        let fields = params.as_object().ok_or("params must be an object")?;
        let allowed: &[&str] = match action {
            "apn.add" => &[
                "name",
                "apn",
                "username",
                "password",
                "auth_mode",
                "pdp_type",
            ],
            "apn.modify" => &[
                "profile_id",
                "name",
                "apn",
                "username",
                "password",
                "auth_mode",
                "pdp_type",
            ],
            "apn.enable" | "apn.delete" => &["profile_id"],
            "apn.set_mode" => &["mode", "profile_id"],
            _ => return Err("unsupported APN action".into()),
        };
        if fields.keys().any(|k| !allowed.contains(&k.as_str())) {
            return Err("unsupported APN parameter".into());
        }
        let mut form = Map::new();
        if action == "apn.set_mode" {
            match params["mode"].as_i64() {
                Some(0) if !fields.contains_key("profile_id") => {
                    form.insert("apn_mode".into(), json!("auto"));
                    return Ok(Plan {
                        form,
                        index: None,
                        name: None,
                        kind: Kind::Auto,
                    });
                }
                Some(1) => {}
                _ => {
                    return Err(
                        "mode must be 0 (auto) or 1 (manual); manual requires profile_id".into(),
                    );
                }
            }
        }
        let index = if action == "apn.add" {
            // The WebUI appends by index. Never reuse a hole or overwrite a record.
            let next = (0..20)
                .rfind(|i| self.records[*i].is_some() || self.v6[*i].is_some())
                .map_or(0, |i| i + 1);
            let limit = self.raw["apn_num_preset"]
                .as_str()
                .and_then(|s| s.parse::<usize>().ok())
                .filter(|n| (1..=20).contains(n))
                .unwrap_or(10);
            if next >= limit {
                return Err("manual APN catalog is full".into());
            }
            next
        } else {
            Self::index(params)?
        };
        let existing = if action == "apn.add" {
            None
        } else {
            Some(self.at(index)?)
        };
        form.insert("apn_mode".into(), json!("manual"));
        form.insert("index".into(), json!(index.to_string()));
        let old_name = existing.map(|r| r[0].clone());
        if action == "apn.delete" {
            // Same guard as the original WebUI, including auto mode's matching name.
            let active = self.raw["profile_name_ui"]
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or("active APN identity unavailable")?;
            if old_name.as_deref() == Some(active) {
                return Err("cannot delete the current APN profile".into());
            }
            form.insert("apn_action".into(), json!("delete"));
            return Ok(Plan {
                form,
                index: Some(index),
                name: old_name,
                kind: Kind::Delete,
            });
        }
        if matches!(action, "apn.enable" | "apn.set_mode") {
            let pdp = &existing.unwrap()[7];
            if !matches!(pdp.as_str(), "IP" | "IPv6" | "IPv4v6") {
                return Err("unknown profile PDP type".into());
            }
            form.insert("apn_action".into(), json!("set_default"));
            form.insert("set_default_flag".into(), json!("1"));
            form.insert("pdp_type".into(), json!(pdp));
            return Ok(Plan {
                form,
                index: Some(index),
                name: old_name,
                kind: Kind::Enable,
            });
        }
        let name = text(params["name"].as_str().ok_or("name is required")?, 64)?;
        let apn = text(params["apn"].as_str().ok_or("apn is required")?, 128)?;
        if name.is_empty() || apn.is_empty() {
            return Err("name and apn must not be empty".into());
        }
        if (0..20).any(|i| i != index && self.at(i).is_ok_and(|r| r[0] == name)) {
            return Err("APN profile name already exists".into());
        }
        let pdp = match params.get("pdp_type") {
            Some(v) => match v.as_u64() {
                Some(1) => "IP",
                Some(2) => "IPv6",
                Some(3) => "IPv4v6",
                _ => return Err("pdp_type must be 1..3".into()),
            },
            None => existing.map(|r| r[7].as_str()).unwrap_or("IPv4v6"),
        };
        if !matches!(pdp, "IP" | "IPv6" | "IPv4v6") {
            return Err("unknown profile PDP type".into());
        }
        for (k, v) in [
            ("apn_action", "save"),
            ("profile_name", name.as_str()),
            ("wan_dial", "*99#"),
            ("apn_select", "manual"),
            ("pdp_type", pdp),
            ("pdp_select", "auto"),
            ("pdp_addr", ""),
        ] {
            form.insert(k.into(), json!(v));
        }
        for (prefix, old) in [
            ("", self.records[index].as_ref()),
            ("ipv6_", self.v6[index].as_ref()),
        ] {
            if (prefix.is_empty() && pdp == "IPv6") || (!prefix.is_empty() && pdp == "IP") {
                continue;
            }
            let old = old.filter(|r| !r[7].is_empty());
            let auth = match params.get("auth_mode") {
                Some(v) => match v.as_u64() {
                    Some(0) => "none",
                    Some(1) => "pap",
                    Some(2) => "chap",
                    _ => return Err("OEM auth_mode must be 0 (none), 1 (PAP), or 2 (CHAP)".into()),
                }
                .to_owned(),
                None => old
                    .map(|r| r[4].to_lowercase())
                    .unwrap_or_else(|| "none".into()),
            };
            if !matches!(auth.as_str(), "none" | "pap" | "chap") {
                return Err("unknown stored APN authentication".into());
            }
            let field = |key: &str, pos: usize| -> Result<String, String> {
                match params.get(key) {
                    Some(v) => text(v.as_str().ok_or("APN credentials must be strings")?, 128),
                    None => text(old.map(|r| r[pos].as_str()).unwrap_or(""), 128),
                }
            };
            let dns = old
                .map(|r| r[10].as_str())
                .filter(|v| !v.is_empty())
                .unwrap_or("auto");
            if !matches!(dns, "auto" | "manual") {
                return Err("unknown stored DNS mode".into());
            }
            for (k, v) in [
                ("wan_apn", apn.clone()),
                ("ppp_auth_mode", auth),
                ("ppp_username", field("username", 5)?),
                ("ppp_passwd", field("password", 6)?),
                ("dns_mode", dns.to_owned()),
                (
                    "prefer_dns_manual",
                    text(old.map(|r| r[11].as_str()).unwrap_or(""), 64)?,
                ),
                (
                    "standby_dns_manual",
                    text(old.map(|r| r[12].as_str()).unwrap_or(""), 64)?,
                ),
            ] {
                form.insert(format!("{prefix}{k}"), json!(v));
            }
        }
        Ok(Plan {
            form,
            index: Some(index),
            name: Some(name),
            kind: Kind::Save,
        })
    }
}

enum Kind {
    Auto,
    Enable,
    Save,
    Delete,
}
pub struct Plan {
    pub form: Map<String, Value>,
    pub index: Option<usize>,
    name: Option<String>,
    kind: Kind,
}
impl Plan {
    pub fn verified(&self, after: &Catalog) -> bool {
        match self.kind {
            Kind::Auto => after.raw["apn_mode"] == "auto",
            Kind::Enable => {
                after.raw["apn_mode"] == "manual"
                    && after.raw["profile_name_ui"].as_str() == self.name.as_deref()
                    && after.raw["pdp_type_ui"] == self.form["pdp_type"]
            }
            Kind::Delete => !(0..20).any(|i| {
                after
                    .at(i)
                    .is_ok_and(|r| Some(r[0].as_str()) == self.name.as_deref())
            }),
            Kind::Save => {
                let i = self.index.unwrap();
                [("", &after.records[i]), ("ipv6_", &after.v6[i])]
                    .iter()
                    .all(|(prefix, record)| {
                        if !self.form.contains_key(&format!("{prefix}wan_apn")) {
                            return true;
                        }
                        let Some(r) = record else {
                            return false;
                        };
                        r[0] == self.name.as_deref().unwrap_or("")
                            && r[7] == self.form["pdp_type"].as_str().unwrap_or("")
                            && [
                                ("wan_apn", 1),
                                ("ppp_auth_mode", 4),
                                ("ppp_username", 5),
                                ("ppp_passwd", 6),
                                ("dns_mode", 10),
                                ("prefer_dns_manual", 11),
                                ("standby_dns_manual", 12),
                            ]
                            .iter()
                            .all(|(k, pos)| {
                                let expected =
                                    self.form[&format!("{prefix}{k}")].as_str().unwrap_or("");
                                if *k == "ppp_auth_mode" {
                                    r[*pos].eq_ignore_ascii_case(expected)
                                } else if *k == "dns_mode" && r[*pos].is_empty() {
                                    expected == "auto"
                                } else {
                                    r[*pos] == expected
                                }
                            })
                    })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Catalog {
        let mut raw =
            json!({"apn_interface_version":"2","apn_mode":"manual","profile_name_ui":"Current"});
        for i in 0..20 {
            for p in ["APN_config", "ipv6_APN_config"] {
                raw[format!("{p}{i}")] = json!("");
            }
        }
        raw["APN_config0"] =
            json!("Current($)net($)manual($)*99#($)none($)($)($)IP($)auto($)($)($)($)");
        raw["APN_config1"] = json!(
            "Other($)net($)manual($)*99#($)PAP($)user4($)secret4($)IPv4v6($)auto($)($)manual($)1.1.1.1($)8.8.8.8"
        );
        raw["ipv6_APN_config1"] = json!(
            "Other($)net($)manual($)*99#($)CHAP($)user6($)secret6($)IPv4v6($)auto($)($)manual($)2606:4700:4700::1111($)"
        );
        Catalog::parse(raw).unwrap()
    }
    #[test]
    fn modify_preserves_each_family_credentials_and_dns() {
        let c = fixture();
        let p = c
            .plan(
                "apn.modify",
                &json!({"profile_id":"manual-1","name":"Other","apn":"new.net"}),
            )
            .unwrap();
        assert_eq!(p.form["ppp_passwd"], "secret4");
        assert_eq!(p.form["ipv6_ppp_passwd"], "secret6");
        assert_eq!(p.form["ipv6_ppp_auth_mode"], "chap");
        assert_eq!(p.form["prefer_dns_manual"], "1.1.1.1");
        assert_eq!(p.form["ipv6_prefer_dns_manual"], "2606:4700:4700::1111");
        assert!(!p.verified(&c));
        let mut after = c.raw.clone();
        for key in ["APN_config1", "ipv6_APN_config1"] {
            after[key] = json!(after[key].as_str().unwrap().replacen(
                "($)net($)",
                "($)new.net($)",
                1
            ));
        }
        assert!(p.verified(&Catalog::parse(after).unwrap()));
    }
    #[test]
    fn create_appends_and_never_selects_profile() {
        let p = fixture()
            .plan(
                "apn.add",
                &json!({"name":"New","apn":"new.net","pdp_type":2,"auth_mode":0}),
            )
            .unwrap();
        assert_eq!(p.index, Some(2));
        assert!(!p.form.contains_key("wan_apn"));
        assert_eq!(p.form["ipv6_wan_apn"], "new.net");
        assert!(!p.form.contains_key("set_default_flag"));
    }
    #[test]
    fn rejects_ambiguous_ids_malformed_fields_and_current_delete() {
        let c = fixture();
        for id in [
            "current",
            "automatic-1",
            "manual-01",
            "manual-20",
            "manual--1",
            "0",
        ] {
            assert!(c.plan("apn.delete", &json!({"profile_id":id})).is_err());
        }
        assert!(
            c.plan("apn.delete", &json!({"profile_id":"manual-0"}))
                .is_err()
        );
        for p in [
            json!({"name":"Current","apn":"a"}),
            json!({"name":"x($)y","apn":"a"}),
            json!({"name":"x","apn":"a","pdp_type":0}),
            json!({"name":"x","apn":"a","auth_mode":3}),
            json!({"name":"x","apn":"a","password":null}),
            json!({"name":"x","apn":"a","index":0}),
        ] {
            assert!(c.plan("apn.add", &p).is_err());
        }
        assert!(c.plan("apn.set_mode", &json!({"mode":1})).is_err());
        assert!(c.plan("apn.set_mode", &json!({"mode":"auto"})).is_err());
    }
    #[test]
    fn rejects_incomplete_catalog_and_retains_explicit_empty_password() {
        let c = fixture();
        let p = c
            .plan(
                "apn.modify",
                &json!({"profile_id":"manual-1","name":"Other","apn":"net","password":""}),
            )
            .unwrap();
        assert_eq!(p.form["ppp_passwd"], "");
        assert_eq!(p.form["ipv6_ppp_passwd"], "");
        let mut raw = c.raw;
        raw.as_object_mut().unwrap().remove("APN_config19");
        assert!(Catalog::parse(raw).is_err());
    }
    #[test]
    fn delete_verifies_identity_not_old_index_after_compaction() {
        let c = fixture();
        let p = c
            .plan("apn.delete", &json!({"profile_id":"manual-1"}))
            .unwrap();
        assert!(!p.verified(&c));
        let mut raw = c.raw;
        raw["APN_config1"] = json!("");
        raw["ipv6_APN_config1"] = json!("");
        assert!(p.verified(&Catalog::parse(raw).unwrap()));
    }
}
