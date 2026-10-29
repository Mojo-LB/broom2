use super::*;

#[test]
fn names() {
    assert!(valid_name("w11-lounge") && valid_name("a") && valid_name("ubuntu2604"));
    assert!(!valid_name(""), "empty");
    assert!(!valid_name("W11"), "upper case");
    assert!(!valid_name("w11_x"), "underscore");
    assert!(!valid_name("-w11"), "leading dash");
    assert!(!valid_name(&"a".repeat(25)), "too long");
    assert!(valid_iso("Win11_25H2_English_x64.iso"));
    assert!(valid_iso("ubuntu-26.04-desktop-amd64.ISO"));
    assert!(!valid_iso("win11.img"), "not .iso");
    assert!(!valid_iso("../x.iso"), "path");
    assert!(!valid_iso("a b.iso"), "space");
    assert!(!valid_iso(".iso"), "hidden / empty name");
}

#[test]
fn os_defaults_to_windows() {
    let g: Golden = serde_json::from_str(r#"{"name":"w11","size_gb":256}"#).unwrap();
    assert_eq!((g.os.as_str(), g.iso, g.maint), ("windows", None, None));
    let i: Iso = serde_json::from_str(r#"{"name":"x.iso","size":1}"#).unwrap();
    assert_eq!(i.os, "windows");
    assert!(os_ok(" Linux ").is_ok_and(|o| o == "linux"));
    assert!(os_ok("macos").is_err());
}

/// A golden's machines: in one of its groups (none = no machine: diskless is never the default) and with a fixed IP.
#[test]
fn machines_of_a_golden() {
    let m = |id: i64, grp: Option<&str>, ip: Option<&str>| Machine {
        id,
        mac: format!("02:00:00:00:00:{id:02x}"),
        ip: ip.map(Into::into),
        grp: grp.map(Into::into),
        ..Default::default()
    };
    let all = vec![
        m(1, Some("Lounge"), Some("10.0.0.1")),
        m(2, Some("lounge"), None),
        m(3, Some("VIP"), Some("10.0.0.3")),
        m(4, None, Some("10.0.0.4")),
        m(5, Some("lounge"), Some("")),
    ];
    let g = |groups: &[&str]| Golden {
        name: "w11".into(),
        os: "windows".into(),
        size_gb: 256,
        iso: None,
        groups: groups.iter().map(|s| s.to_string()).collect(),
        maint: None,
        ram: false,
    };
    let ids = |g: &Golden| {
        machines_of(g, &all)
            .iter()
            .map(|m| m.id)
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(&g(&["LOUNGE"])), [1], "group (any case), fixed IP only");
    assert_eq!(ids(&g(&["lounge", "VIP"])), [1, 3]);
    assert!(ids(&g(&[])).is_empty(), "no group: no machine");
}

#[test]
fn places_are_known() {
    assert_eq!(sub_of("golden"), Some("goldens"));
    assert_eq!(sub_of("writeback"), Some("diskless"));
    assert_eq!(sub_of("iso"), Some("isos"));
    assert_eq!(sub_of("games"), None);
}

/// The open boot URLs act for a machine only when the request comes from it: its IP, its MAC behind it.
#[test]
fn boot_requests_only_from_the_machine_itself() {
    let m = Machine {
        id: 7,
        mac: "02:00:00:00:00:07".into(),
        ip: Some("10.77.0.21".into()),
        hostname: Some("PC07".into()),
        ..Default::default()
    };
    let arp = |x: Option<&'static str>| move || x.map(String::from);
    let ip = m.ip.as_deref();
    assert!(allowed_peer(&m, ip, "10.77.0.21", arp(Some("02-00-00-00-00-07"))).is_ok());
    assert!(
        allowed_peer(&m, ip, "10.77.0.22", arp(Some("02:00:00:00:00:07"))).is_err(),
        "other IP"
    );
    assert!(
        allowed_peer(&m, ip, "10.77.0.21", arp(Some("02:00:00:00:00:08"))).is_err(),
        "other card"
    );
    assert!(
        allowed_peer(&m, ip, "10.77.0.21", arp(None)).is_err(),
        "no ARP entry"
    );
    assert!(
        allowed_peer(&m, None, "10.77.0.21", arp(Some("02:00:00:00:00:07"))).is_err(),
        "no IP"
    );
}

/// A machine that maintains a golden and is also in a diskless group boots the maintenance entry by default.
#[test]
fn maintenance_entry_wins_the_default() {
    let e = |t: &str, d: bool| MenuEntry {
        text: t.into(),
        chain: String::new(),
        default: d,
    };
    let out = ordered(
        vec![e("Install cdtest", true), e("Maintain cdtest", false)],
        vec![e("w11 (diskless)", true), e("w10 (diskless)", true)],
    );
    let got: Vec<(&str, bool)> = out.iter().map(|x| (x.text.as_str(), x.default)).collect();
    assert_eq!(
        got,
        [
            ("Install cdtest", true),
            ("Maintain cdtest", false),
            ("w11 (diskless)", false),
            ("w10 (diskless)", false)
        ]
    );
    let only = ordered(
        vec![],
        vec![e("w11 (diskless)", true), e("w10 (diskless)", true)],
    );
    assert_eq!(
        (only[0].default, only[1].default),
        (true, false),
        "first diskless one"
    );
}
