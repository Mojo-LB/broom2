// A whole room switched on at the same moment: 45 diskless PCs, each with its own network card and fixed IP, ask the
// server for everything a boot needs at once — iPXE over TFTP, the boot menu, their diskless boot (their disk made
// ready: last session dropped), the iSCSI login to their own disk, reads of the golden, writes of their own. Every PC
// is answered on its first request: none is refused, sent back to the menu or made to queue behind another PC, and
// each one's writes are its own. A PC that asks twice at once (reset while its boot was being prepared) is served too.
mod common;

use common::clients::{DISCOVER, Dhcp, Http, Iscsi, OFFER, tftp_get_from};
use common::*;
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

/// PCs in the room (BROOM_ROOM_PCS for a run by hand, at most 80).
const PCS: usize = 45;
/// The golden's first megabytes hold a pattern (the rest of its 16 GB reads zeros): what every PC reads
/// (BROOM_ROOM_MB for a run by hand).
const GOLDEN_MB: usize = 4;

fn setting(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// What one PC's boot took, step by step.
struct Boot {
    tftp: Duration,
    menu: Duration,
    /// The diskless boot request: the server's own work for this PC.
    prepare: Duration,
    login: Duration,
    /// The golden's pattern read + this PC's own block written and read back.
    disk: Duration,
    total: Duration,
}

/// The PC's whole boot, as its firmware and iPXE do it. Panics on anything but being served at once.
fn boot(pc: &Pc, efi: &[u8], golden: &[u8]) -> (Boot, Iscsi) {
    let t0 = Instant::now();
    let got =
        tftp_get_from(pc.ip, "snponly.efi").unwrap_or_else(|e| panic!("{}: TFTP: {e}", pc.name));
    assert!(got == efi, "{}: iPXE over TFTP", pc.name);
    let tftp = t0.elapsed();

    let http = Http::pc(pc.ip);
    let (mac, ip) = (&pc.mac, pc.ip);
    let t = Instant::now();
    let menu = http.get(&format!("/boot.ipxe?mac={mac}&ip={ip}")).text();
    assert!(
        menu.contains("/boot/diskless?golden=w11"),
        "{}: its menu offers the diskless golden: {menu}",
        pc.name
    );
    let menu_t = t.elapsed();

    let t = Instant::now();
    let script = http
        .get(&format!("/boot/diskless?golden=w11&mac={mac}&ip={ip}"))
        .text();
    assert!(
        served(&script),
        "{}: served on its first request, not sent back to the menu: {script}",
        pc.name
    );
    let prepare = t.elapsed();

    let t = Instant::now();
    let mut disk = Iscsi::login_from(pc.ip, &iqn_of(&script))
        .unwrap_or_else(|e| panic!("{}: iSCSI login refused: {e:?}", pc.name));
    let login = t.elapsed();

    let t = Instant::now();
    // The golden, 256 KB a read.
    for (i, want) in golden.chunks(256 << 10).enumerate() {
        let lba = (i * (256 << 10) / 512) as u32;
        assert!(
            disk.read(lba, (want.len() / 512) as u16) == want,
            "{}: golden at LBA {lba}",
            pc.name
        );
    }
    // Its own block, over the golden's first 64 KB: kept on the server, for this PC only.
    let mine = own_block(pc);
    disk.write(0, &mine);
    assert!(disk.read(0, 128) == mine, "{}: its own write", pc.name);
    (
        Boot {
            tftp,
            menu: menu_t,
            prepare,
            login,
            disk: t.elapsed(),
            total: t0.elapsed(),
        },
        disk,
    )
}

/// The answer to a diskless boot request is the PC's boot (its disk on the server), not a message and the menu again.
fn served(script: &str) -> bool {
    script.contains("echo Booting w11 from the server")
        && script.contains("sanboot --drive 0x80 iscsi:")
}

/// 64 KB only this PC writes (its number in every byte).
fn own_block(pc: &Pc) -> Vec<u8> {
    vec![pc.ip.octets()[3]; 64 << 10]
}

fn ms(d: Duration) -> u128 {
    d.as_millis()
}

/// "name: median / slowest" of one step over all PCs.
fn line(name: &str, all: &[Boot], f: impl Fn(&Boot) -> Duration) -> String {
    let mut v: Vec<Duration> = all.iter().map(f).collect();
    v.sort();
    format!(
        "  {name:<28} median {:>5} ms   slowest {:>5} ms",
        ms(v[v.len() / 2]),
        ms(v[v.len() - 1])
    )
}

#[test]
fn a_room_boots_at_once() {
    sandbox("a_room_boots_at_once", || {
        let home = Home::new("room");
        let _srv = Server::start(&home);
        let h = admin(&home);
        configure_network(&h);
        let pcs = Arc::new(add_pcs(setting("BROOM_ROOM_PCS", PCS)));

        // The room: every PC registered with its fixed IP, in one group; a Windows golden for that group.
        let ids: Vec<i64> = pcs
            .iter()
            .map(|pc| {
                let r = h.post(
                    "/api/machines",
                    &serde_json::json!({"mac": pc.mac, "ip": pc.ip.to_string(), "hostname": pc.name}),
                );
                assert_eq!(r.status, 200, "add {}: {}", pc.name, r.text());
                r.json()["id"].as_i64().unwrap()
            })
            .collect();
        let post = |path: &str, body: serde_json::Value| {
            let r = h.post(path, &body);
            assert_eq!(r.status, 200, "{path}: {}", r.text());
        };
        post("/api/groups/create", serde_json::json!({"name": "room"}));
        post(
            "/api/groups/set",
            serde_json::json!({"name": "room", "machines": ids}),
        );
        let mb = setting("BROOM_ROOM_MB", GOLDEN_MB);
        let golden = Arc::new(pattern(mb << 20, 7));
        let img = home.0.join("goldens/w11/golden.img");
        std::fs::create_dir_all(img.parent().unwrap()).unwrap();
        std::fs::write(&img, &golden[..]).unwrap();
        post(
            "/api/diskless/golden",
            serde_json::json!({"name": "w11", "os": "windows", "size_gb": 16, "groups": ["room"]}),
        );
        let efi = Arc::new(
            std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/ipxe/snponly.efi")).unwrap(),
        );

        // DHCP, the very first step of every PC's firmware: each one is offered its own fixed IP (its disk answers
        // only that address), all of them in a row without a pause.
        let dhcp = Dhcp::open();
        let pxe: [(u8, &[u8]); 2] = [(60, b"PXEClient:Arch:00007:UNDI:003016"), (93, &[0, 7])];
        let t = Instant::now();
        for (i, pc) in pcs.iter().enumerate() {
            let offer = dhcp.exchange(DISCOVER, pc.mac6, 0x5000 + i as u32, &pxe);
            assert_eq!(
                (offer.msg_type(), offer.yiaddr),
                (OFFER, pc.ip),
                "{}: its fixed IP",
                pc.name
            );
        }
        let dhcp_all = t.elapsed();

        // One PC alone first: what a boot takes with the server to itself.
        let (alone, disk) = boot(&pcs[0], &efi, &golden);
        drop(disk);

        // The whole room at the same moment (the first PC again: a reboot).
        let wave = |what: &str| {
            let start = Arc::new(Barrier::new(pcs.len()));
            let t0 = Instant::now();
            let threads: Vec<_> = (0..pcs.len())
                .map(|i| {
                    let (pcs, efi, golden, start) =
                        (pcs.clone(), efi.clone(), golden.clone(), start.clone());
                    std::thread::spawn(move || {
                        start.wait();
                        boot(&pcs[i], &efi, &golden)
                    })
                })
                .collect();
            let (boots, mut disks): (Vec<Boot>, Vec<Iscsi>) = threads
                .into_iter()
                .map(|t| t.join().expect("a PC of the room was not served"))
                .unzip();
            let took = t0.elapsed();
            // All of them are up together, each on its own disk: the golden with its own block on top.
            for (pc, disk) in pcs.iter().zip(disks.iter_mut()) {
                assert!(disk.read(0, 128) == own_block(pc), "{}: its block", pc.name);
                assert!(
                    disk.read(128, 128) == golden[64 << 10..128 << 10],
                    "{}: the golden under it",
                    pc.name
                );
            }
            assert!(
                !home.log().contains("diskless boot refused"),
                "no PC was turned away"
            );
            println!("  {what}: all {} served in {} ms", pcs.len(), ms(took));
            println!("{}", line("iPXE over TFTP", &boots, |b| b.tftp));
            println!("{}", line("boot menu", &boots, |b| b.menu));
            println!("{}", line("boot prepared (server)", &boots, |b| b.prepare));
            println!("{}", line("iSCSI login", &boots, |b| b.login));
            println!("{}", line("golden read + own write", &boots, |b| b.disk));
            println!("{}", line("whole boot", &boots, |b| b.total));
            // Side by side, not one after the other: each PC does the same work as the one alone, with this host's
            // few CPUs for all of them and the server — far from any time a PC would spend waiting for its turn.
            let slowest = boots.iter().map(|b| b.total).max().unwrap();
            assert!(
                slowest < Duration::from_secs(30),
                "a PC of the room waited {} ms for its boot",
                ms(slowest)
            );
            disks
        };
        println!(
            "room of {} PCs, {mb} MB read each (this host: {} CPUs)",
            pcs.len(),
            cpus()
        );
        println!(
            "  one PC alone: TFTP {} ms, menu {} ms, boot prepared {} ms, login {} ms, disk {} ms, whole boot {} ms",
            ms(alone.tftp),
            ms(alone.menu),
            ms(alone.prepare),
            ms(alone.login),
            ms(alone.disk),
            ms(alone.total)
        );
        println!(
            "  DHCP: {} PCs offered their fixed IP in {} ms",
            pcs.len(),
            ms(dhcp_all)
        );
        let up = wave("switched on at once");
        // The room's power goes and comes back (or every PC is reset): all ask again at once while the server still
        // holds their sessions. Each one's old session is closed and dropped, and it boots — again without a detour.
        let mut disks = wave("reset at once while running");
        for (pc, mut old) in pcs.iter().zip(up) {
            assert!(old.closed(), "{}: its session of before the reset", pc.name);
        }

        // A PC reset while its boot is being prepared asks again: both requests are answered with its boot, the
        // second after the first (its disk made ready again) — not sent back to the menu.
        drop(disks.remove(1));
        let pc = &pcs[1];
        let again: Vec<_> = (0..2)
            .map(|_| {
                let (mac, ip) = (pc.mac.clone(), pc.ip);
                std::thread::spawn(move || {
                    Http::pc(ip)
                        .get(&format!("/boot/diskless?golden=w11&mac={mac}&ip={ip}"))
                        .text()
                })
            })
            .collect();
        for t in again {
            let script = t.join().unwrap();
            assert!(served(&script), "asked twice at once: {script}");
        }
        // Its disk is fresh again (the last session dropped), the others keep theirs.
        let script = Http::pc(pc.ip)
            .get(&format!(
                "/boot/diskless?golden=w11&mac={}&ip={}",
                pc.mac, pc.ip
            ))
            .text();
        let mut disk = Iscsi::login_from(pc.ip, &iqn_of(&script)).expect("login after a reboot");
        assert!(disk.read(0, 128) == golden[..64 << 10], "a fresh session");
        assert!(
            disks[1].read(0, 128) == own_block(&pcs[2]),
            "others untouched"
        );

        // Another PC's address does not get this PC's boot or disk.
        let other = Http::pc(pcs[3].ip)
            .get(&format!(
                "/boot/diskless?golden=w11&mac={}&ip={}",
                pc.mac, pc.ip
            ))
            .text();
        assert!(!served(&other) && !other.contains("iscsi:"), "{other}");
        assert!(
            Iscsi::login_from(pcs[3].ip, &iqn_of(&script)).is_err(),
            "its disk answers only that PC"
        );
    });
}

fn cpus() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get())
}
