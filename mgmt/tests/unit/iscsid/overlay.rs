use super::*;

fn base_of(data: &[u8]) -> impl Fn(u64, &mut [u8]) -> Result<(), String> + '_ {
    move |off, buf| {
        buf.copy_from_slice(&data[off as usize..off as usize + buf.len()]);
        Ok(())
    }
}

#[test]
fn reads_see_writes_over_the_disk() {
    let d = std::env::temp_dir().join("broom_test_overlay");
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let size = 5 * BLOCK + 1000; // a partial last block
    let base: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
    let b = base_of(&base);
    let o = Overlay::open(&d.join("u.ovl"), &d.join("u.blocks"), size).unwrap();
    // Unaligned write across a block boundary: two blocks taken, the rest of each copied from the disk.
    o.write_at(&b, BLOCK - 10, &[7u8; 30]).unwrap();
    o.write_at(&b, BLOCK + 100, &[9u8; 4]).unwrap();
    assert_eq!(o.used(), 2 * BLOCK);
    let mut want = base.clone();
    want[(BLOCK - 10) as usize..(BLOCK + 20) as usize].fill(7);
    want[(BLOCK + 100) as usize..(BLOCK + 104) as usize].fill(9);
    let mut got = vec![0u8; size as usize];
    o.read_at(&b, 0, &mut got).unwrap();
    assert!(got == want);
    // Neighbouring blocks on one side are one call: [0, 1] in the overlay, [2..=5] below → 1 call to the disk.
    let calls = std::cell::Cell::new(0);
    let counted = |off: u64, buf: &mut [u8]| {
        calls.set(calls.get() + 1);
        b(off, buf)
    };
    o.read_at(&counted, 0, &mut got).unwrap();
    assert!(got == want);
    assert_eq!(calls.get(), 1);
    o.write_at(&b, size - 1, &[3]).unwrap(); // the partial last block
    assert!(o.write_at(&b, size - 1, &[1, 2]).is_err(), "past the end");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn survives_reopen_and_merges() {
    let d = std::env::temp_dir().join("broom_test_overlay_kept");
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let size = 4 * BLOCK;
    let base_path = d.join("games.img");
    std::fs::write(&base_path, vec![1u8; size as usize]).unwrap();
    let base = std::fs::read(&base_path).unwrap();
    let (ovl, list) = (d.join("update.ovl"), d.join("update.blocks"));
    let o = Overlay::open(&ovl, &list, size).unwrap();
    o.write_at(&base_of(&base), 2 * BLOCK + 5, b"game").unwrap();
    o.flush().unwrap();
    drop(o);
    // Reopened (daemon restart): the block is still the overlay's.
    let o = Overlay::open(&ovl, &list, size).unwrap();
    let mut buf = [0u8; 4];
    o.read_at(&base_of(&base), 2 * BLOCK + 5, &mut buf).unwrap();
    assert_eq!(&buf, b"game");
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(&base_path)
        .unwrap();
    assert_eq!(o.merge_into(&f).unwrap(), 1);
    let merged = std::fs::read(&base_path).unwrap();
    assert_eq!(
        &merged[(2 * BLOCK + 5) as usize..(2 * BLOCK + 9) as usize],
        b"game"
    );
    assert_eq!(
        merged.iter().filter(|&&x| x != 1).count(),
        4,
        "only those bytes changed"
    );
    let _ = std::fs::remove_dir_all(&d);
}

/// Room for a diskless machine's writes (Limit): its own limit; the drive's reserve stops the big writers first and
/// lets the small ones go on; nobody below the floor; an unknown drive is not held against the machine.
#[test]
fn room_rules() {
    const GB: u64 = 1 << 30;
    let l = |max_gb: u64, free_pct: u8| Limit {
        max: max_gb * GB,
        free_pct,
        dir: "/".into(),
    };
    let drive = |free_gb: u64| Some((free_gb * GB, 1000 * GB));
    // No limit of its own, plenty of room: a 300 GB game install goes through.
    assert!(room(&l(0, 5), 300 * GB, drive(500)).is_ok());
    // Its own limit: up to it, not past it.
    assert!(room(&l(100, 5), 100 * GB - BLOCK, drive(500)).is_ok());
    assert!(room(&l(100, 5), 100 * GB, drive(500)).is_err());
    assert!(room(&l(100, 0), 100 * GB, None).is_err(), "its own limit needs no drive figures");
    // Below the reserve (5 % of 1000 GB = 50 GB): the big writer stops, an ordinary session goes on.
    assert!(room(&l(0, 5), FAIR, drive(49)).is_err());
    assert!(room(&l(0, 5), 300 * GB, drive(49)).is_err());
    assert!(room(&l(0, 5), FAIR - BLOCK, drive(49)).is_ok());
    assert!(room(&l(0, 5), 0, drive(49)).is_ok());
    assert!(room(&l(0, 5), 300 * GB, drive(50)).is_ok(), "exactly at the reserve: fine");
    // No reserve set: only the floor stops it.
    assert!(room(&l(0, 0), 300 * GB, drive(2)).is_ok());
    // Under the floor nobody grows, however small.
    assert!(room(&l(0, 5), 0, Some((FLOOR - 1, 1000 * GB))).is_err());
    assert!(room(&l(0, 0), 0, Some((FLOOR - 1, 1000 * GB))).is_err());
    // The drive's figures unknown: not refused for it.
    assert!(room(&l(0, 5), 300 * GB, None).is_ok());
}

/// A limited overlay: new blocks only within its limit (refused as "no room", noted once); blocks it already holds
/// are rewritten freely, reads are untouched, and an overlay without a limit (a games disk update) is as before.
#[test]
fn limited_overlay_refuses_new_blocks_only() {
    let d = std::env::temp_dir().join(format!("broom_test_ovl_limit_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let size = 16 * BLOCK;
    let base = |_: u64, b: &mut [u8]| -> Result<(), String> {
        b.fill(0x11);
        Ok(())
    };
    let ov = Overlay::open_limited(
        &d.join("ovl"),
        &d.join("blocks"),
        size,
        Some(Limit {
            max: 2 * BLOCK,
            free_pct: 0,
            dir: d.clone(),
        }),
    )
    .unwrap();
    ov.write_at(&base, 0, &[0xaa; 100]).unwrap();
    ov.write_at(&base, BLOCK + 5, &[0xbb; 100]).unwrap();
    assert_eq!(ov.used(), 2 * BLOCK);
    let e = ov.write_at(&base, 5 * BLOCK, &[0xcc; 10]).unwrap_err();
    assert!(e.starts_with(NO_ROOM), "{e}");
    assert!(ov.first_refusal(), "the first refusal is the one logged");
    assert!(!ov.first_refusal());
    assert_eq!(ov.used(), 2 * BLOCK, "nothing added");
    // What it holds is rewritten, and reads as written; the refused block reads from below.
    ov.write_at(&base, 10, &[0xdd; 20]).unwrap();
    let mut buf = [0u8; 4];
    ov.read_at(&base, 10, &mut buf).unwrap();
    assert_eq!(buf, [0xdd; 4]);
    ov.read_at(&base, 5 * BLOCK, &mut buf).unwrap();
    assert_eq!(buf, [0x11; 4]);
    // Without a limit: as ever.
    let free = Overlay::open(&d.join("ovl2"), &d.join("blocks2"), size).unwrap();
    for b in 0..16 {
        free.write_at(&base, b * BLOCK, &[1; 8]).unwrap();
    }
    assert_eq!(free.used(), size);
    let _ = std::fs::remove_dir_all(&d);
}
