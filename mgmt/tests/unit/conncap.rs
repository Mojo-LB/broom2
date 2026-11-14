use super::*;

fn ip(s: &str) -> IpAddr {
    s.parse().unwrap()
}

/// One address gets its share and no more; a place is free again when a connection ends; the others are not
/// affected.
#[test]
fn one_address_cannot_hold_them_all() {
    let c = Caps::new("test", 3, 100);
    let a: Vec<Held> = (0..3).map(|_| c.admit(ip("10.0.0.5")).unwrap()).collect();
    assert!(c.admit(ip("10.0.0.5")).is_none(), "its fourth");
    assert!(
        c.admit(ip("::ffff:10.0.0.5")).is_none(),
        "the same address, written as IPv6"
    );
    let b = c.admit(ip("10.0.0.6")).expect("another address");
    assert_eq!(c.open(), (4, 2));
    drop(a);
    assert_eq!(c.open(), (1, 1), "given back, and the address forgotten");
    assert!(c.admit(ip("10.0.0.5")).is_some());
    drop(b);
    assert_eq!(c.open(), (0, 0));
}

/// All addresses together stay under the total; the server's own address is held to the total only.
#[test]
fn all_together_and_the_server_itself() {
    let c = Caps::new("test", 2, 5);
    let own: Vec<Held> = (0..4).map(|_| c.admit(ip("127.0.0.1")).unwrap()).collect();
    let one = c.admit(ip("10.0.0.7")).expect("the fifth");
    assert!(c.admit(ip("10.0.0.8")).is_none(), "the total is reached");
    assert!(
        c.admit(ip("127.0.0.1")).is_none(),
        "for the server itself too"
    );
    drop(one);
    assert!(c.admit(ip("10.0.0.8")).is_some());
    drop(own);
}
