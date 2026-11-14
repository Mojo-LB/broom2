use super::{KEEP, Notice, put};

/// A repeat of the same key is one line (the newest text and time, counted); other keys keep their order; only the
/// newest KEEP stay.
#[test]
fn repeats_merge_and_the_list_stays_short() {
    let (mut list, mut next) = (Vec::<Notice>::new(), 0u64);
    put(&mut list, &mut next, "room:aa", "PC01 ran out of room", 10);
    put(&mut list, &mut next, "boot:bb", "PC02 refused", 11);
    put(
        &mut list,
        &mut next,
        "room:aa",
        "PC01 ran out of room again",
        12,
    );
    assert_eq!(list.len(), 2);
    assert_eq!((list[0].key.as_str(), list[0].n), ("boot:bb", 1));
    assert_eq!(
        (list[1].text.as_str(), list[1].at, list[1].n),
        ("PC01 ran out of room again", 12, 2)
    );
    assert!(
        list[1].id > list[0].id,
        "a repeat gets a new id (it shows again)"
    );
    for i in 0..(KEEP as u64 + 5) {
        put(&mut list, &mut next, &format!("k{i}"), "x", 20 + i);
    }
    assert_eq!(list.len(), KEEP);
    assert_eq!(list.last().unwrap().key, format!("k{}", KEEP + 4));
    // A long text is cut, not refused.
    put(&mut list, &mut next, "long", &"y".repeat(1000), 99);
    assert_eq!(list.last().unwrap().text.len(), 400);
}
