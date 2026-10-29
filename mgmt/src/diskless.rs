// diskless.rs — diskless goldens: whole-disk images (Windows by default, or Linux) that machines with no local disk boot
// straight from over iSCSI (the daemon side is iscsid/golden.rs).
//
// A golden starts blank. Its maintenance machine boots an installer from the ISO library with the golden attached,
// installs the OS, drivers and software onto it, then boots the golden itself to configure it; Save makes that work the
// golden's next version (every machine picks it up at its next boot), Discard throws it away. Each machine of the
// golden's groups (fixed IP required: its disk answers only that IP and MAC) gets its own layers on the writeback
// volume: base = its first-boot setup, kept while the version stays; session = everything else, wiped every boot.
//
// Where things live — each on a volume chosen on the Diskless page ("" = broom's own folder):
//   golden_volume     <vol>/goldens/<name>/golden.img (+ its versions' layers)
//   writeback_volume  <vol>/diskless/<name>/<mac>/   (the machines' layers)
//   iso_volume        <vol>/isos/<file>.iso           (the installer library)
// Lists in the config table: diskless_goldens, isos (JSON).
use axum::{
    Json, Router,
    body::Body,
    extract::{Query, State},
    http::StatusCode,
    routing::{get, post, put},
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::SharedState;
use crate::api::{ApiError, bad, ise, ok};
use crate::db::Machine;
use crate::iscsid::golden;
use crate::machines::{clean_groups, for_group, machine_by_id, who};

pub fn routes() -> Router<SharedState> {
    Router::new()
        .route("/api/diskless", get(status))
        .route("/api/diskless/golden", post(upsert))
        .route("/api/diskless/golden/delete", post(delete))
        .route("/api/diskless/maint", post(set_maint))
        .route("/api/diskless/save", post(save))
        .route("/api/diskless/discard", post(discard))
        .route("/api/diskless/volume", post(set_volume))
        .route("/api/diskless/iso/upload-start", post(iso_start))
        .route(
            "/api/diskless/iso/upload-chunk",
            put(iso_chunk).layer(axum::extract::DefaultBodyLimit::max(
                crate::images::CHUNK_MAX,
            )),
        )
        .route("/api/diskless/iso/upload-done", post(iso_done))
        .route("/api/diskless/iso/os", post(iso_os))
        .route("/api/diskless/iso/delete", post(iso_delete))
}

pub const OSES: [&str; 2] = ["windows", "linux"];

fn default_os() -> String {
    "windows".into()
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Golden {
    pub name: String,
    #[serde(default = "default_os")]
    pub os: String,
    pub size_gb: u64,
    /// Installer ISO (file name in the library) its maintenance machine boots to install the OS.
    #[serde(default)]
    pub iso: Option<String>,
    /// Groups whose machines boot it diskless; empty = none.
    #[serde(default)]
    pub groups: Vec<String>,
    /// Its maintenance machine (machine id).
    #[serde(default)]
    pub maint: Option<i64>,
    #[serde(default)]
    pub ram: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Iso {
    /// File name in the library.
    pub name: String,
    #[serde(default = "default_os")]
    pub os: String,
    pub size: u64,
    /// Unix seconds.
    #[serde(default)]
    pub added: u64,
}

pub(crate) fn goldens(st: &SharedState) -> Vec<Golden> {
    serde_json::from_str(&st.db.get_config("diskless_goldens", "[]")).unwrap_or_default()
}

fn edit<R>(
    st: &SharedState,
    f: impl FnOnce(&mut Vec<Golden>) -> Result<R, ApiError>,
) -> Result<R, ApiError> {
    crate::db::edit_json(&*st.db, "diskless_goldens", f).map_err(ise)?
}

pub(crate) fn isos(st: &SharedState) -> Vec<Iso> {
    serde_json::from_str(&st.db.get_config("isos", "[]")).unwrap_or_default()
}

fn edit_isos<R>(
    st: &SharedState,
    f: impl FnOnce(&mut Vec<Iso>) -> Result<R, ApiError>,
) -> Result<R, ApiError> {
    crate::db::edit_json(&*st.db, "isos", f).map_err(ise)?
}

fn not_found(name: &str) -> ApiError {
    (StatusCode::NOT_FOUND, format!("golden {name} not found"))
}

/// 1-24 lower-case letters / digits / '-' (part of a target name and of folder names).
pub(crate) fn valid_name(n: &str) -> bool {
    (1..=24).contains(&n.len())
        && n.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !n.starts_with('-')
}

/// A plain file name ending in .iso: letters, digits, '.', '_', '-' (it is joined onto the library folder).
pub(crate) fn valid_iso(n: &str) -> bool {
    (5..=128).contains(&n.len())
        && n.to_ascii_lowercase().ends_with(".iso")
        && !n.starts_with('.')
        && n.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn os_ok(os: &str) -> Result<String, ApiError> {
    let os = os.trim().to_ascii_lowercase();
    if OSES.contains(&os.as_str()) {
        Ok(os)
    } else {
        Err(bad("os: windows or linux"))
    }
}

// ---------- where things live ----------

/// The three places, with their sub-folder on a volume.
pub(crate) const PLACES: [(&str, &str); 3] = [
    ("golden", "goldens"),
    ("writeback", "diskless"),
    ("iso", "isos"),
];

fn sub_of(kind: &str) -> Option<&'static str> {
    PLACES.iter().find(|(k, _)| *k == kind).map(|(_, s)| *s)
}

/// Volume id chosen for `kind` ("" = broom's own folder).
fn volume_of(st: &SharedState, kind: &str) -> String {
    st.db.get_config(&format!("{kind}_volume"), "")
}

/// Folder of `kind` ("golden" | "writeback" | "iso").
pub(crate) fn dir_of(st: &SharedState, kind: &str) -> Result<PathBuf, String> {
    let sub = sub_of(kind).ok_or_else(|| format!("unknown place {kind}"))?;
    match volume_of(st, kind).as_str() {
        "" => Ok(crate::home().join(sub)),
        id => crate::volumes::folder_on(st, id, sub),
    }
}

fn golden_img(st: &SharedState, name: &str) -> Result<PathBuf, String> {
    Ok(dir_of(st, "golden")?.join(name).join("golden.img"))
}

/// The golden's target name prefix: "<iqn_base>:golden-<name>"; version k = ".g<k>", machine = ".m<mac>".
pub(crate) fn iqn(st: &SharedState, name: &str) -> String {
    let base = st.db.get_config("iqn_base", "iqn.2026-01.local.broom");
    format!("{base}:golden-{name}")
}

/// Machine `m`'s own target on golden `name`.
pub(crate) fn machine_target(st: &SharedState, name: &str, m: &Machine) -> Option<String> {
    golden::machine_iqn(&iqn(st, name), &m.mac)
}

/// The machines that boot golden `g` diskless: in one of its groups, with a fixed IP (their disk answers only it).
pub(crate) fn machines_of<'a>(g: &Golden, all: &'a [Machine]) -> Vec<&'a Machine> {
    all.iter()
        .filter(|m| !g.groups.is_empty() && for_group(&g.groups, m.grp.as_deref()))
        .filter(|m| m.ip.as_deref().is_some_and(|ip| !ip.is_empty()))
        .collect()
}

/// IP of a machine: its fixed IP, else its DHCP lease.
fn ip_of(st: &SharedState, m: &Machine) -> Option<String> {
    m.ip.clone().filter(|i| !i.is_empty()).or_else(|| {
        st.db
            .leases()
            .ok()?
            .into_iter()
            .find(|l| l.mac.eq_ignore_ascii_case(&m.mac))
            .and_then(|l| l.ip)
    })
}

fn configs(st: &SharedState) -> Result<Vec<golden::Config>, String> {
    let wb = dir_of(st, "writeback")?;
    let all = st.db.machines().map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for g in goldens(st) {
        let path = golden_img(st, &g.name)?;
        if !path.exists() {
            continue;
        }
        let maint = g.maint.and_then(|id| all.iter().find(|m| m.id == id));
        out.push(golden::Config {
            disk: crate::iscsid::games::Config {
                name: g.name.clone(),
                iqn: iqn(st, &g.name),
                path: path.to_string_lossy().into(),
                update_ip: maint.and_then(|m| ip_of(st, m)),
                update_mac: maint.map(|m| m.mac.to_lowercase()),
                ram: g.ram,
                reserve: crate::publish::ram_reserve(st),
            },
            dir: wb.join(&g.name).to_string_lossy().into(),
            machines: machines_of(&g, &all)
                .into_iter()
                .map(|m| golden::Machine {
                    mac: m.mac.clone(),
                    ip: m.ip.clone().unwrap_or_default(),
                })
                .collect(),
        });
    }
    Ok(out)
}

/// Why the goldens are not served (last sync); None = fine.
static LAST_ERR: parking_lot::Mutex<Option<String>> = parking_lot::Mutex::new(None);

/// Push the goldens to the iSCSI daemon (at start, on change, every minute: a maintenance machine's lease may move).
/// Blocking.
pub fn sync(st: &SharedState) -> Result<(), String> {
    let r = configs(st).and_then(|list| {
        if list.is_empty() && crate::iscsi::diskless_status().is_none() {
            Ok(()) // nothing to serve, no daemon to tell
        } else {
            crate::iscsi::diskless_set(list)
        }
    });
    let mut last = LAST_ERR.lock();
    match &r {
        Err(e) if last.as_deref() != Some(e.as_str()) => {
            tracing::warn!("diskless goldens not served: {e}")
        }
        Ok(()) if last.is_some() => tracing::info!("diskless goldens served again"),
        _ => {}
    }
    *last = r.as_ref().err().cloned();
    r
}

async fn sync_now(st: SharedState) -> Result<(), ApiError> {
    tokio::task::spawn_blocking(move || sync(&st))
        .await
        .map_err(ise)?
        .map_err(|e| (StatusCode::CONFLICT, e))
}

/// Groups page: every golden's groups through `f` (name, groups → new groups), as one step. No group = no machine.
pub(crate) fn edit_groups(
    st: &SharedState,
    f: &dyn Fn(&str, &[String]) -> Vec<String>,
) -> Result<(), ApiError> {
    edit(st, |list| {
        for g in list.iter_mut() {
            g.groups = f(&g.name, &g.groups);
        }
        Ok(())
    })
}

/// Machine `m` was deleted: no longer any golden's maintenance machine, its layers of every golden deleted. Blocking.
pub(crate) fn forget_machine(st: &SharedState, m: &Machine) {
    let all = goldens(st);
    let _ = edit(st, |list| {
        for g in list.iter_mut().filter(|g| g.maint == Some(m.id)) {
            g.maint = None;
        }
        Ok(())
    });
    if all.is_empty() {
        return;
    }
    let _ = sync(st);
    for g in &all {
        if let Err(e) = crate::iscsi::diskless_forget(&g.name, &m.mac) {
            tracing::warn!("golden {}: layers of {} not deleted: {e}", g.name, who(m));
        }
    }
}

// ---------- boot (boot.rs) ----------

/// A boot menu entry: its text, the URL it chains to (mac / ip appended), and whether it is the default.
pub(crate) struct MenuEntry {
    pub text: String,
    pub chain: String,
    pub default: bool,
}

/// Target name of library ISO `iso` served as a DVD-ROM: "<iqn_base>:iso-<name>" (lower-case letters / digits / '-').
pub(crate) fn iso_iqn(st: &SharedState, iso: &str) -> String {
    let base = st.db.get_config("iqn_base", "iqn.2026-01.local.broom");
    let n: String = iso
        .trim_end_matches(".iso")
        .trim_end_matches(".ISO")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    format!("{base}:iso-{n}")
}

/// The golden's state in the daemon (None = not served).
fn served(name: &str) -> Option<golden::Info> {
    crate::iscsi::diskless_status()
        .unwrap_or_default()
        .into_iter()
        .find(|i| i.disk.name == name)
}

/// Nothing written to it yet: no version saved, no maintenance work — a blank disk to install onto.
fn blank(i: &golden::Info) -> bool {
    i.disk.ver == 0 && i.disk.layers == 0 && i.disk.update_bytes == 0
}

/// Machine `m`'s entries: Install / Maintain for each golden it maintains (Install is the default while the golden is
/// blank, Maintain once the installer wrote to it — the installer's own restarts go on by themselves), then each golden
/// it boots diskless (the first is the default unless it maintains one). Blocking (asks the daemon).
pub(crate) fn menu_entries(st: &SharedState, m: &Machine) -> Vec<MenuEntry> {
    let (mut maint, mut diskless): (Vec<MenuEntry>, Vec<MenuEntry>) = (Vec::new(), Vec::new());
    let all = st.db.machines().unwrap_or_default();
    for g in goldens(st) {
        let Some(info) = served(&g.name) else {
            continue;
        };
        if g.maint == Some(m.id) {
            let out = &mut maint;
            let fresh = blank(&info);
            if let Some(iso) = &g.iso {
                out.push(MenuEntry {
                    text: format!("Install {} from {iso} (maintenance)", g.name),
                    chain: format!("/boot/maint?golden={}&do=install", g.name),
                    default: fresh,
                });
            }
            out.push(MenuEntry {
                text: format!("Maintain {} (maintenance, saved with Save)", g.name),
                chain: format!("/boot/maint?golden={}&do=boot", g.name),
                default: !fresh || g.iso.is_none(),
            });
        } else if machines_of(&g, &all).iter().any(|x| x.id == m.id) {
            diskless.push(MenuEntry {
                text: format!("{} (diskless)", g.name),
                chain: format!("/boot/diskless?golden={}", g.name),
                default: true,
            });
        }
    }
    ordered(maint, diskless)
}

/// Maintenance entries first (its work is why it is on), then diskless ones; one default at most: the first marked.
pub(crate) fn ordered(mut maint: Vec<MenuEntry>, mut diskless: Vec<MenuEntry>) -> Vec<MenuEntry> {
    maint.append(&mut diskless);
    let mut seen = false;
    for e in maint.iter_mut().filter(|e| e.default) {
        if seen {
            e.default = false;
        }
        seen = true;
    }
    maint
}

/// The request comes from machine `m` itself: from its IP, with its MAC behind it (ARP) — these boot URLs are open
/// and reset its session / close its sessions, so another PC must not trigger them for it.
fn from_machine(m: &Machine, ip: Option<&str>, peer: &str) -> Result<(), String> {
    allowed_peer(m, ip, peer, || crate::license::arp_mac(peer))
}

pub(crate) fn allowed_peer(
    m: &Machine,
    ip: Option<&str>,
    peer: &str,
    arp: impl FnOnce() -> Option<String>,
) -> Result<(), String> {
    if ip != Some(peer) {
        return Err(format!(
            "{} must boot from its own IP ({}), not {peer}",
            who(m),
            ip.unwrap_or("none")
        ));
    }
    let seen = arp().and_then(|a| golden::mac12(&a));
    if seen.is_none() || seen != golden::mac12(&m.mac) {
        return Err(format!("{peer} is not {}'s network card", who(m)));
    }
    Ok(())
}

fn iscsi_uri(st: &SharedState, iqn: &str) -> String {
    format!("iscsi:{}::::{iqn}", st.db.get_config("dhcp_server_ip", ""))
}

/// An iPXE script that boots `body`, else says why and goes back to the menu.
fn boot_script(body: &str, what: &str) -> String {
    format!(
        "#!ipxe\n{body}\n:failed\necho Could not boot {what} from the server\nsleep 5\n\
         chain /boot.ipxe?mac=${{net0/mac}}&ip=${{net0/ip}}\n"
    )
}

/// Machine `m` boots golden `name` diskless: its earlier sessions closed, its session layer reset (its base kept while
/// the golden version stays), then iPXE boots its own target (the iBFT tells Windows / Linux where its disk is).
/// Blocking.
pub(crate) fn boot_diskless(
    st: &SharedState,
    name: &str,
    m: &Machine,
    peer: &str,
) -> Result<String, String> {
    from_machine(m, m.ip.as_deref(), peer)?;
    let g = goldens(st)
        .into_iter()
        .find(|g| g.name == name)
        .ok_or_else(|| format!("no golden {name}"))?;
    let all = st.db.machines().map_err(|e| e.to_string())?;
    if !machines_of(&g, &all).iter().any(|x| x.id == m.id) {
        return Err(format!(
            "golden {name} is not for this machine (its groups, a fixed IP)"
        ));
    }
    if let Some(ip) = m.ip.as_deref() {
        let _ = crate::iscsi::kick_peer(ip); // its previous boot's sessions (it is in its boot menu now)
    }
    let mut p = None;
    for i in 0..30 {
        match crate::iscsi::diskless_prepare(name, &m.mac, false, false) {
            Ok(x) => {
                p = Some(x);
                break;
            }
            Err(e) if e.contains("still connected") && i < 29 => {
                std::thread::sleep(std::time::Duration::from_millis(200))
            }
            Err(e) => return Err(e),
        }
    }
    let p = p.ok_or("still connected")?;
    let t = machine_target(st, name, m).ok_or("bad MAC")?;
    tracing::info!(
        "golden {name}: {} boots diskless on g{}{}",
        who(m),
        p.ver,
        if p.first {
            " (first boot on this version)"
        } else {
            ""
        }
    );
    Ok(boot_script(
        &format!(
            "echo Booting {name} from the server (diskless)...\nsanboot --drive 0x80 {} || goto failed",
            iscsi_uri(st, &t)
        ),
        name,
    ))
}

/// Maintenance machine `m` works on golden `name`: `install` = the golden attached (blank disk) and the golden's
/// installer ISO booted as a DVD; `boot` = the golden itself. Both writable for it only (the daemon checks its IP + MAC);
/// Save makes the work the golden's next version. Blocking.
pub(crate) fn boot_maint(
    st: &SharedState,
    name: &str,
    what: &str,
    m: &Machine,
    peer: &str,
) -> Result<String, String> {
    from_machine(m, ip_of(st, m).as_deref(), peer)?;
    let g = goldens(st)
        .into_iter()
        .find(|g| g.name == name)
        .ok_or_else(|| format!("no golden {name}"))?;
    if g.maint != Some(m.id) {
        return Err(format!(
            "{} is not the maintenance machine of {name}",
            who(m)
        ));
    }
    let info = served(name).ok_or_else(|| format!("golden {name} is not served"))?;
    let disk = iscsi_uri(st, &format!("{}.g{}", iqn(st, name), info.disk.ver));
    if let Some(ip) = ip_of(st, m) {
        let _ = crate::iscsi::kick_peer(&ip); // its previous boot's sessions: their writes are flushed
    }
    match what {
        "install" => {
            let iso = g
                .iso
                .clone()
                .ok_or("no installer ISO chosen for this golden")?;
            let path = dir_of(st, "iso")?.join(&iso);
            if !path.is_file() {
                return Err(format!("ISO {iso} is not in the library folder"));
            }
            let t = iso_iqn(st, &iso);
            crate::iscsi::export_cd(&t, &path.to_string_lossy())?;
            tracing::info!("golden {name}: {} installs from {iso}", who(m));
            Ok(boot_script(
                &format!(
                    "echo Installing onto {name} from {iso}\n\
                     echo If the installer asks to press a key to boot from CD/DVD, press one now.\n\
                     sanhook --drive 0x80 {disk} || goto failed\n\
                     sanboot --drive 0x81 {} || goto failed",
                    iscsi_uri(st, &t)
                ),
                &iso,
            ))
        }
        "boot" => {
            tracing::info!("golden {name}: {} boots it for maintenance", who(m));
            Ok(boot_script(
                &format!(
                    "echo Booting {name} for maintenance (writable)...\nsanboot --drive 0x80 {disk} || goto failed"
                ),
                name,
            ))
        }
        _ => Err("do: install or boot".into()),
    }
}

// ---------- API ----------

/// GET /api/diskless — goldens (with what the daemon reports), ISO library, the three places, machines (pickers).
async fn status(State(st): State<SharedState>) -> Result<Json<serde_json::Value>, ApiError> {
    let daemon = tokio::task::spawn_blocking(crate::iscsi::diskless_status)
        .await
        .map_err(ise)?
        .unwrap_or_default();
    let all = st.db.machines().map_err(ise)?;
    let list: Vec<serde_json::Value> = goldens(&st)
        .into_iter()
        .map(|g| {
            let d = daemon.iter().find(|i| i.disk.name == g.name);
            let without_ip: Vec<String> = all
                .iter()
                .filter(|m| !g.groups.is_empty() && for_group(&g.groups, m.grp.as_deref()))
                .filter(|m| m.ip.as_deref().is_none_or(str::is_empty))
                .map(who)
                .collect();
            let targets: Vec<serde_json::Value> = machines_of(&g, &all)
                .into_iter()
                .map(|m| serde_json::json!({"name": who(m), "mac": m.mac, "ip": m.ip, "target": machine_target(&st, &g.name, m)}))
                .collect();
            serde_json::json!({
                "golden": g,
                "iqn": iqn(&st, &g.name),
                "machines": targets,
                "served": d.is_some(),
                "status": d,
                "machines_without_fixed_ip": without_ip,
            })
        })
        .collect();
    let places: serde_json::Map<String, serde_json::Value> = PLACES
        .iter()
        .map(|(k, _)| {
            (
                k.to_string(),
                serde_json::json!({
                    "volume": volume_of(&st, k),
                    "path": dir_of(&st, k).ok().map(|p| p.to_string_lossy().into_owned()),
                }),
            )
        })
        .collect();
    let machines: Vec<serde_json::Value> = all
        .iter()
        .map(|m| serde_json::json!({"id": m.id, "name": who(m), "mac": m.mac, "ip": m.ip, "group": m.grp}))
        .collect();
    Ok(Json(serde_json::json!({
        "goldens": list,
        "isos": isos(&st),
        "places": places,
        "machines": machines,
        "error": *LAST_ERR.lock(),
    })))
}

#[derive(Deserialize)]
struct GoldenBody {
    name: String,
    #[serde(default = "default_os")]
    os: String,
    size_gb: u64,
    #[serde(default)]
    iso: Option<String>,
    #[serde(default)]
    groups: Vec<String>,
}

/// POST /api/diskless/golden {name, os=windows, size_gb, iso, groups} — a new blank golden, or change one (same name).
/// golden.img is sparse and can only grow; its OS can't change once created.
async fn upsert(
    State(st): State<SharedState>,
    Json(b): Json<GoldenBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let name = b.name.trim().to_ascii_lowercase();
    if !valid_name(&name) {
        return Err(bad("name: 1-24 lower-case letters / digits / '-'"));
    }
    let os = os_ok(&b.os)?;
    if !(16..=64 << 10).contains(&b.size_gb) {
        return Err(bad("size: 16 GB to 64 TB"));
    }
    let groups = clean_groups(&b.groups).map_err(bad)?;
    let iso = b.iso.filter(|i| !i.is_empty());
    if let Some(i) = &iso {
        let lib = isos(&st);
        let Some(x) = lib.iter().find(|x| &x.name == i) else {
            return Err(bad(format!("ISO {i} is not in the library")));
        };
        if x.os != os {
            return Err(bad(format!("ISO {i} is a {} installer, not {os}", x.os)));
        }
    }
    let p = golden_img(&st, &name).map_err(bad)?;
    let old = goldens(&st).into_iter().find(|g| g.name == name);
    if let Some(o) = &old
        && o.os != os
    {
        return Err(bad(format!(
            "golden {name} is {}: create another one",
            o.os
        )));
    }
    let (want, have) = (
        b.size_gb << 30,
        std::fs::metadata(&p).map_or(0, |m| m.len()),
    );
    if want < have {
        return Err(bad(format!(
            "golden {name} is {} GB: it can only grow",
            have >> 30
        )));
    }
    if want > have {
        std::fs::create_dir_all(p.parent().unwrap()).map_err(ise)?;
        let f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&p)
            .map_err(ise)?;
        f.set_len(want).map_err(ise)?;
        tracing::info!("golden {name}: {} GB ({})", b.size_gb, p.display());
    }
    edit(&st, |list| {
        let g = Golden {
            name: name.clone(),
            os: os.clone(),
            size_gb: b.size_gb,
            iso: iso.clone(),
            groups: groups.clone(),
            maint: old.as_ref().and_then(|o| o.maint),
            ram: old.as_ref().is_some_and(|o| o.ram),
        };
        match list.iter().position(|x| x.name == name) {
            Some(i) => list[i] = g,
            None => list.push(g),
        }
        Ok(())
    })?;
    sync_now(st).await?;
    Ok(ok())
}

#[derive(Deserialize)]
struct NameBody {
    name: String,
}

fn find(st: &SharedState, name: &str) -> Result<Golden, ApiError> {
    goldens(st)
        .into_iter()
        .find(|g| g.name == name)
        .ok_or_else(|| not_found(name))
}

/// Daemon's view of golden `name`: nobody on it, no unsaved maintenance work.
fn idle(name: &str) -> Result<(), String> {
    let Some(i) = crate::iscsi::diskless_status()
        .unwrap_or_default()
        .into_iter()
        .find(|i| i.disk.name == name)
    else {
        return Ok(());
    };
    let on = i.machines.iter().filter(|m| m.connected).count() + i.disk.sessions;
    if on > 0 {
        return Err(format!("{on} machine(s) use it — shut them down first"));
    }
    if i.disk.update_connected || i.disk.update_bytes > 0 {
        return Err("its maintenance work is not saved — Save or Discard it first".into());
    }
    Ok(())
}

/// POST /api/diskless/golden/delete {name} — the golden, its versions and every machine's layers of it.
async fn delete(
    State(st): State<SharedState>,
    Json(b): Json<NameBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    find(&st, &b.name)?;
    let n = b.name.clone();
    tokio::task::spawn_blocking(move || idle(&n))
        .await
        .map_err(ise)?
        .map_err(|e| (StatusCode::CONFLICT, e))?;
    let dirs = [
        golden_img(&st, &b.name)
            .map_err(ise)?
            .parent()
            .unwrap()
            .to_path_buf(),
        dir_of(&st, "writeback").map_err(ise)?.join(&b.name),
    ];
    edit(&st, |list| {
        list.retain(|g| g.name != b.name);
        Ok(())
    })?;
    sync_now(st).await?;
    tokio::task::spawn_blocking(move || {
        for d in dirs {
            if let Err(e) = crate::volumes::remove_tree(&d)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                return Err(format!("{}: {e}", d.display()));
            }
        }
        Ok(())
    })
    .await
    .map_err(ise)?
    .map_err(ise)?;
    tracing::info!("golden {} deleted", b.name);
    Ok(ok())
}

#[derive(Deserialize)]
struct MaintBody {
    name: String,
    /// The maintenance machine; None = maintenance off.
    id: Option<i64>,
}

/// POST /api/diskless/maint {name, id} — the machine whose next boots install / maintain the golden (writable).
async fn set_maint(
    State(st): State<SharedState>,
    Json(b): Json<MaintBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let w = match b.id {
        Some(id) => Some(who(&machine_by_id(&st, id)?)),
        None => None,
    };
    edit(&st, |list| {
        let g = list
            .iter_mut()
            .find(|g| g.name == b.name)
            .ok_or_else(|| not_found(&b.name))?;
        g.maint = b.id;
        Ok(())
    })?;
    match w {
        Some(w) => tracing::info!("golden {}: {w} is its maintenance machine", b.name),
        None => tracing::info!("golden {}: maintenance off", b.name),
    }
    sync_now(st).await?;
    Ok(ok())
}

/// Save / Discard refused because the daemon still counts the maintenance machine as connected: Windows powers off
/// without logging out of iSCSI and TCP notices a dead client only after ~1 min. No agent and no ping answer = off →
/// its sessions are closed (the daemon flushes its writes) and the step runs again. Blocking.
fn with_maint_off<T>(
    st: &SharedState,
    name: &str,
    step: impl Fn() -> Result<T, String>,
) -> Result<T, String> {
    let e = match step() {
        Err(e) if e.contains("still connected") => e,
        r => return r,
    };
    let Some(m) = find(st, name)
        .ok()
        .and_then(|g| g.maint)
        .and_then(|id| machine_by_id(st, id).ok())
    else {
        return Err(e);
    };
    let Some(ip) = ip_of(st, &m) else {
        return Err(e);
    };
    let w = who(&m);
    if crate::vnc::connected(&m.mac) {
        return Err(format!(
            "the maintenance machine {w} is still on (its agent is connected) — shut it down first"
        ));
    }
    if crate::monitor::ping(&ip) {
        return Err(format!(
            "the maintenance machine {w} still answers on the network — wait for it to power off"
        ));
    }
    let n = crate::iscsi::kick_peer(&ip).map_err(|_| e.clone())?;
    tracing::info!(
        "golden {name}: maintenance machine {w} is off — its {n} iSCSI session(s) closed"
    );
    for _ in 0..20 {
        let busy = crate::iscsi::diskless_status()
            .unwrap_or_default()
            .iter()
            .any(|i| i.disk.name == name && i.disk.update_connected);
        if !busy {
            return step();
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    Err(e)
}

/// POST /api/diskless/save {name} — the maintenance work becomes the golden's next version (machine must be off).
async fn save(
    State(st): State<SharedState>,
    Json(b): Json<NameBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    find(&st, &b.name)?;
    let (name, st2) = (b.name.clone(), st.clone());
    let msg = tokio::task::spawn_blocking(move || {
        with_maint_off(&st2, &name, || crate::iscsi::golden_save(&name))
    })
    .await
    .map_err(ise)?
    .map_err(|e| (StatusCode::CONFLICT, e))?;
    tracing::info!("golden {}: {msg}", b.name);
    Ok(Json(serde_json::json!({"ok": true, "status": msg})))
}

/// POST /api/diskless/discard {name} — throw the maintenance work away.
async fn discard(
    State(st): State<SharedState>,
    Json(b): Json<NameBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    find(&st, &b.name)?;
    let (name, st2) = (b.name.clone(), st.clone());
    tokio::task::spawn_blocking(move || {
        with_maint_off(&st2, &name, || crate::iscsi::golden_discard(&name))
    })
    .await
    .map_err(ise)?
    .map_err(|e| (StatusCode::CONFLICT, e))?;
    tracing::info!("golden {}: maintenance work discarded", b.name);
    Ok(ok())
}

#[derive(Deserialize)]
struct VolumeBody {
    /// "golden" | "writeback" | "iso".
    kind: String,
    /// Volume id; "" = broom's own folder.
    #[serde(default)]
    id: String,
}

/// POST /api/diskless/volume {kind, id} — where goldens / the machines' layers / the ISO library live. Only while that
/// place is empty (nothing is moved).
async fn set_volume(
    State(st): State<SharedState>,
    Json(b): Json<VolumeBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let sub = sub_of(&b.kind).ok_or_else(|| bad("kind: golden, writeback or iso"))?;
    let id = b.id.trim().to_string();
    if id == volume_of(&st, &b.kind) {
        return Ok(ok());
    }
    let busy = match b.kind.as_str() {
        "iso" => !isos(&st).is_empty(),
        _ => !goldens(&st).is_empty(),
    };
    if busy {
        return Err(bad(format!(
            "the {} place holds data already: delete it first (moving is not supported yet)",
            b.kind
        )));
    }
    let to = match id.as_str() {
        "" => crate::home().join(sub),
        v => crate::volumes::folder_on(&st, v, sub).map_err(bad)?,
    };
    std::fs::create_dir_all(&to).map_err(|e| bad(format!("{}: {e}", to.display())))?;
    st.db
        .set_config(&format!("{}_volume", b.kind), &id)
        .map_err(ise)?;
    tracing::info!("diskless: {} now at {}", b.kind, to.display());
    Ok(ok())
}

// ---------- ISO library ----------

fn iso_upload_dir(st: &SharedState) -> Result<PathBuf, ApiError> {
    Ok(dir_of(st, "iso").map_err(bad)?.join(".upload"))
}

#[derive(Deserialize)]
struct IsoStart {
    name: String,
    #[serde(default = "default_os")]
    os: String,
}

/// POST /api/diskless/iso/upload-start {name, os=windows} — begin uploading an installer ISO (drops a half-done one).
async fn iso_start(
    State(st): State<SharedState>,
    Json(b): Json<IsoStart>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !valid_iso(&b.name) {
        return Err(bad(
            "ISO name: letters, digits, '.', '_', '-', ending in .iso",
        ));
    }
    os_ok(&b.os)?;
    if isos(&st).iter().any(|i| i.name == b.name) {
        return Err(bad(format!("{} is already in the library", b.name)));
    }
    let dir = iso_upload_dir(&st)?.join(&b.name);
    let _ = tokio::fs::remove_dir_all(&dir).await;
    tokio::fs::create_dir_all(&dir).await.map_err(ise)?;
    Ok(ok())
}

#[derive(Deserialize)]
struct IsoChunk {
    name: String,
    offset: u64,
    total: u64,
}

/// PUT /api/diskless/iso/upload-chunk?name=&offset=&total= — up to 16 MB of the ISO at `offset`, in any order.
async fn iso_chunk(
    State(st): State<SharedState>,
    Query(q): Query<IsoChunk>,
    body: Body,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !valid_iso(&q.name) {
        return Err(bad("bad ISO name"));
    }
    let dir = iso_upload_dir(&st)?.join(&q.name);
    crate::images::write_chunk(dir, &q.name, q.offset, q.total, 64 << 30, body).await?;
    Ok(ok())
}

/// POST /api/diskless/iso/upload-done {name, os} — the upload joins the library.
async fn iso_done(
    State(st): State<SharedState>,
    Json(b): Json<IsoStart>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !valid_iso(&b.name) {
        return Err(bad("bad ISO name"));
    }
    let os = os_ok(&b.os)?;
    let up = iso_upload_dir(&st)?.join(&b.name);
    let src = up.join(&b.name);
    let size = std::fs::metadata(&src)
        .map_err(|_| bad("no upload of it in progress"))?
        .len();
    let dst = dir_of(&st, "iso").map_err(bad)?.join(&b.name);
    std::fs::rename(&src, &dst).map_err(ise)?;
    let _ = std::fs::remove_dir_all(&up);
    let _ = std::fs::remove_dir(up.parent().unwrap_or(&up)); // .upload itself, once no other upload runs
    let added = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    edit_isos(&st, |l| {
        l.retain(|i| i.name != b.name);
        l.push(Iso {
            name: b.name.clone(),
            os: os.clone(),
            size,
            added,
        });
        Ok(())
    })?;
    tracing::info!(
        "ISO library: {} added ({os}, {:.1} GB)",
        b.name,
        size as f64 / 1e9
    );
    Ok(ok())
}

#[derive(Deserialize)]
struct IsoOs {
    name: String,
    os: String,
}

/// POST /api/diskless/iso/os {name, os} — correct an ISO's OS.
async fn iso_os(
    State(st): State<SharedState>,
    Json(b): Json<IsoOs>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let os = os_ok(&b.os)?;
    edit_isos(&st, |l| {
        let i = l
            .iter_mut()
            .find(|i| i.name == b.name)
            .ok_or_else(|| bad(format!("{} is not in the library", b.name)))?;
        i.os = os;
        Ok(())
    })?;
    Ok(ok())
}

/// POST /api/diskless/iso/delete {name} — refused while a golden names it as its installer.
async fn iso_delete(
    State(st): State<SharedState>,
    Json(b): Json<NameBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if let Some(g) = goldens(&st)
        .iter()
        .find(|g| g.iso.as_deref() == Some(b.name.as_str()))
    {
        return Err(bad(format!("golden {} installs from it", g.name)));
    }
    edit_isos(&st, |l| {
        let n = l.len();
        l.retain(|i| i.name != b.name);
        if l.len() == n {
            return Err(bad(format!("{} is not in the library", b.name)));
        }
        Ok(())
    })?;
    if valid_iso(&b.name) {
        let _ = std::fs::remove_file(dir_of(&st, "iso").map_err(ise)?.join(&b.name));
    }
    tracing::info!("ISO library: {} deleted", b.name);
    Ok(ok())
}

#[cfg(test)]
#[path = "../tests/unit/diskless.rs"]
mod tests;
