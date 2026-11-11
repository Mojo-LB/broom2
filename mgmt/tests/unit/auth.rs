use super::*;
use axum::http::Method;

#[test]
fn host_header() {
    for ok in [
        "10.0.0.12",
        "10.0.0.12:80",
        "192.168.1.5",
        "[::1]:80",
        "[fe80::1]",
        "localhost:8080",
        "broom",
        "BROOM.local",
    ] {
        assert!(host_allowed(ok, "broom"), "{ok}");
    }
    for bad in [
        "evil.example.com",
        "evil.example.com:80",
        "10.0.0.12.nip.io",
        "broomx",
    ] {
        assert!(!host_allowed(bad, "broom"), "{bad}");
    }
    assert!(
        !host_allowed("anything", ""),
        "no hostname known → only IPs / localhost"
    );
}

#[test]
fn public_paths() {
    let pub_get = |p| is_public(&Method::GET, p);
    let pub_post = |p| is_public(&Method::POST, p);
    assert!(
        pub_get("/login") && pub_get("/login.js"),
        "the standalone sign-in page + its script are public"
    );
    assert!(
        pub_get("/app.css")
            && pub_get("/bootstrap.min.css")
            && !pub_get("/bootstrap.bundle.min.js"),
        "its stylesheets too; the app's scripts are not"
    );
    assert!(pub_get("/boot.ipxe") && pub_get("/tftp/broom/x/vmlinuz"));
    assert!(pub_post("/api/license") && pub_post("/api/auth/login") && pub_post("/api/booted"));
    assert!(!pub_get("/api/golden-chunk"), "delta chunks are gone");
    assert!(
        pub_get("/broom-prep-win") && !pub_post("/api/images/base-mode"),
        "prep script open, admin APIs not"
    );
    // guarded now: the app shell + its fragments (login is a separate page)
    assert!(!pub_get("/") && !pub_get("/machines") && !pub_get("/ui/images"));
    assert!(!pub_get("/api/status") && !pub_get("/api/images") && !pub_post("/api/images/delete"));
    assert!(!pub_post("/api/dhcp") && !pub_post("/api/machines/import") && !pub_get("/api/events"));
    assert!(
        !pub_post("/api/password"),
        "changing the password needs a session"
    );
}

#[test]
fn throttle_per_ip() {
    let mut f = Fails::new();
    let ip: IpAddr = "10.0.0.5".parse().unwrap();
    let other: IpAddr = "10.0.0.6".parse().unwrap();
    for _ in 0..4 {
        record(&mut f, ip, false, 100);
        assert_eq!(locked_for(&f, ip, 100), 0, "first tries are free");
    }
    record(&mut f, ip, false, 100);
    assert_eq!(locked_for(&f, ip, 100), 1, "5th failure locks 1 s");
    record(&mut f, ip, false, 101);
    assert_eq!(locked_for(&f, ip, 101), 2, "then doubles");
    assert_eq!(locked_for(&f, ip, 103), 0, "lock expires");
    assert_eq!(locked_for(&f, other, 101), 0, "other IPs unaffected");
    for _ in 0..30 {
        record(&mut f, ip, false, 200);
    }
    assert_eq!(locked_for(&f, ip, 200), MAX_LOCK_SECS, "capped");
    record(&mut f, ip, true, 1000);
    assert!(f.is_empty(), "success forgets the IP");
    record(&mut f, ip, false, 0);
    record(&mut f, other, false, 10_000);
    assert!(!f.contains_key(&ip), "idle entries dropped");
}

/// Open handshakes: one client can't fill the table (its own oldest goes), a full table drops its oldest instead of
/// refusing everyone; IPv6 counts per /64, IPv4-mapped per IPv4 address.
#[test]
fn handshakes_per_client() {
    let ip = |s: &str| hs_source(s.parse().unwrap());
    let p = |from, until| Pending {
        user: "admin".into(),
        cnonce: String::new(),
        snonce: String::new(),
        until,
        from,
    };
    let mut m = HashMap::new();
    let a = ip("10.0.0.5");
    for i in 0..=HS_PER_SOURCE as u64 {
        admit(&mut m, format!("a{i}"), p(a, 100 + i));
    }
    assert_eq!(m.len(), HS_PER_SOURCE);
    assert!(
        !m.contains_key("a0") && m.contains_key("a8"),
        "its own oldest went"
    );
    admit(&mut m, "b".into(), p(ip("10.0.0.6"), 1));
    assert!(
        m.contains_key("b") && m.len() == HS_PER_SOURCE + 1,
        "another client unaffected"
    );

    let mut m = HashMap::new();
    for i in 0..HS_MAX as u64 {
        admit(
            &mut m,
            format!("{i}"),
            p(IpAddr::from([10, 1, (i >> 8) as u8, i as u8]), 100 + i),
        );
    }
    admit(&mut m, "new".into(), p(ip("10.2.0.1"), 9999));
    assert_eq!(m.len(), HS_MAX);
    assert!(
        m.contains_key("new") && !m.contains_key("0"),
        "full: the oldest of all went"
    );

    assert_eq!(ip("2001:db8::1"), ip("2001:db8::ffff:2"), "one /64");
    assert_ne!(ip("2001:db8::1"), ip("2001:db8:0:1::1"));
    assert_ne!(
        ip("::ffff:10.0.0.1"),
        ip("::ffff:10.0.0.2"),
        "IPv4-mapped: per address"
    );
    assert_eq!(ip("::ffff:10.0.0.1"), ip("10.0.0.1"));
}

#[test]
fn setup_token_compare() {
    assert!(same("abc", "abc") && !same("abc", "abd") && !same("abc", ""));
}

/// What the guard lets the built-in staff role do (its table, `needs`, with staff's permissions).
fn staff_may(m: &Method, path: &str) -> bool {
    allowed(
        &Role::Staff,
        &perms_of(&Role::Staff, &Default::default()),
        &needs(m, path),
    )
}

/// Staff: every page, but of the changes only Wake / games Save, Discard / their own password / logout.
#[test]
fn staff_rights() {
    for p in [
        "/api/status",
        "/api/machines/detail",
        "/api/vnc/view",
        "/api/logs",
        "/api/games",
    ] {
        assert!(staff_may(&Method::GET, p), "GET {p}");
    }
    for p in [
        "/api/machines/export.csv",
        "/api/images/export-file",
        "/api/users",
    ] {
        assert!(!staff_may(&Method::GET, p), "GET {p}");
    }
    for p in [
        "/api/wake",
        "/api/games/save",
        "/api/games/discard",
        "/api/password",
        "/api/auth/logout",
    ] {
        assert!(staff_may(&Method::POST, p), "POST {p}");
    }
    for p in [
        "/api/images/delete",
        "/api/volumes/format",
        "/api/dhcp",
        "/api/users",
        "/api/disks/broom",
        "/api/machines/update",
    ] {
        assert!(!staff_may(&Method::POST, p), "POST {p}");
    }
    assert!(!staff_may(&Method::PUT, "/api/images/upload-chunk"));
    assert!(name_ok("thu-ngan") && name_ok("admin_2"));
    assert!(!name_ok("Admin") && !name_ok("a.b") && !name_ok("") && !name_ok(&"x".repeat(33)));
}

/// Roles: the built-in staff role is exactly what staff could do before roles existed — none of the VM, golden
/// maintenance, version or restart / shut-down rights; a role the owner makes gets what it was given and nothing
/// else; and what is not a permission (accounts, roles, settings, images, machines…) stays the owner's whatever a
/// role holds. All of it decided by the guard's table, for every request.
#[test]
fn roles_and_permissions() {
    use std::collections::BTreeMap;
    let get = Method::GET;
    let post = Method::POST;
    let all: Perms = PERMS.iter().map(|(p, _)| *p).collect();
    let none = Perms::new();
    let may = |role: &Role, perms: &Perms, m: &Method, p: &str| allowed(role, perms, &needs(m, p));

    // Built-in staff: Remote, Wake, Games — and none of what the diskless work added.
    let staff = perms_of(&Role::Staff, &BTreeMap::new());
    assert_eq!(
        staff,
        [Perm::Remote, Perm::Wake, Perm::Games]
            .into_iter()
            .collect()
    );
    for p in [
        "/api/vm/start",
        "/api/vm/stop",
        "/api/versions/pin",
        "/api/diskless/maint",
        "/api/diskless/save",
        "/api/diskless/discard",
        "/api/diskless/refresh",
        "/api/onboard/start",
        "/api/onboard/cancel",
        "/api/onboard/retry",
    ] {
        assert!(!may(&Role::Staff, &staff, &post, p), "staff POST {p}");
        assert!(may(&Role::Owner, &none, &post, p), "owner POST {p}");
    }
    assert!(
        !may(&Role::Staff, &staff, &get, "/api/vm/console"),
        "the VM's screen"
    );
    assert!(!may(&Role::Staff, &staff, &get, "/vm-console"));
    assert!(
        may(&Role::Staff, &staff, &get, "/api/vm"),
        "its state: looking"
    );
    // /api/power passes the guard with Wake (turning on); restart / shut down need Power in the handler.
    assert!(may(&Role::Staff, &staff, &post, "/api/power"));
    assert_eq!(crate::power::needed_for("on"), Perm::Wake);
    for a in ["restart", "shutdown", "cancel"] {
        assert_eq!(crate::power::needed_for(a), Perm::Power, "{a}");
    }
    // The VM: games disks only → Vm or Maintain; keeping Windows changes → Maintain alone.
    assert_eq!(
        crate::vmmaint::needed_for(false),
        [Perm::Vm, Perm::Maintain]
    );
    assert_eq!(crate::vmmaint::needed_for(true), [Perm::Maintain]);

    // A role the owner made: what it was given, nothing else.
    let mut custom = BTreeMap::new();
    custom.insert(
        "night".to_string(),
        [Perm::Power, Perm::Wake].into_iter().collect::<Perms>(),
    );
    custom.insert("looker".to_string(), Perms::new());
    let night = Role::Custom("night".into());
    let np = perms_of(&night, &custom);
    assert!(may(&night, &np, &post, "/api/power") && may(&night, &np, &post, "/api/wake"));
    assert!(!may(&night, &np, &get, "/api/vnc/view"), "no Remote");
    assert!(!may(&night, &np, &post, "/api/games/save"));
    assert!(may(&night, &np, &get, "/api/status") && may(&night, &np, &post, "/api/password"));
    let looker = Role::Custom("looker".into());
    let lp = perms_of(&looker, &custom);
    assert!(
        may(&looker, &lp, &get, "/api/machines/detail") && !may(&looker, &lp, &post, "/api/wake")
    );
    // A role that no longer exists: no permission (the account can look).
    let gone = Role::Custom("gone".into());
    assert!(perms_of(&gone, &custom).is_empty());

    // Not permissions at all: the owner's, even for a role holding every permission.
    let every = Role::Custom("every".into());
    for p in [
        "/api/users",
        "/api/users/delete",
        "/api/roles",
        "/api/roles/delete",
        "/api/config/tls",
        "/api/dhcp",
        "/api/diskless/golden",
        "/api/diskless/golden/delete",
        "/api/diskless/volume",
        "/api/diskless/room",
        "/api/versions/drop-oldest",
        "/api/versions/keep-free",
        "/api/images/delete",
        "/api/machines/update",
        "/api/machines/display",
        "/api/groups/set",
        "/api/vnc/rekey",
        "/api/volumes/format",
        "/api/some/route/added/later",
    ] {
        assert!(!may(&every, &all, &post, p), "POST {p}: owner only");
        assert!(may(&Role::Owner, &none, &post, p));
    }
    for p in ["/api/users", "/api/roles", "/api/machines/export.csv"] {
        assert!(!may(&every, &all, &get, p), "GET {p}: owner only");
    }
    assert!(!may(
        &every,
        &all,
        &Method::PUT,
        "/api/diskless/iso/upload-chunk"
    ));
    assert!(!may(&every, &all, &Method::DELETE, "/api/anything"));

    // A refusal says what is missing.
    let msg = denied(&Role::Staff, &needs(&post, "/api/versions/pin"));
    assert!(
        msg.contains("staff") && msg.contains("version each group boots"),
        "{msg}"
    );
    assert!(denied(&Role::Staff, &Need::Owner).starts_with("owner only"));

    // Roles as stored and sent: their names; the two built-in names are never a custom role's.
    assert_eq!(serde_json::to_string(&Role::Owner).unwrap(), "\"owner\"");
    assert_eq!(
        serde_json::from_str::<Role>("\"staff\"").unwrap(),
        Role::Staff
    );
    assert_eq!(serde_json::from_str::<Role>("\"night\"").unwrap(), night);
    assert_eq!(serde_json::to_string(&night).unwrap(), "\"night\"");
    assert!(serde_json::from_str::<Role>("\"Bad Role\"").is_err());
    assert!(serde_json::from_str::<Role>("\"\"").is_err());
    assert!(crate::users::role_name_ok("night-shift"));
    for n in ["owner", "staff", "Night", "a b", ""] {
        assert!(!crate::users::role_name_ok(n), "{n:?}");
    }
    assert_eq!(
        serde_json::to_string(&Perm::Maintain).unwrap(),
        "\"maintain\""
    );
}
