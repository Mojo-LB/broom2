// iscsid/golden.rs — diskless goldens: whole-disk images (Windows, or Linux) that machines without a local disk boot
// straight from over iSCSI (iPXE sanboot + iBFT).
//
// A golden is a layered disk of the games engine (GOLDENS, games.rs): golden.img + its versions (frozen layers) + the
// writes of its maintenance machine (the engine's update overlay → Save makes them the next version, Discard drops
// them). That machine installs the OS, drivers and software straight onto the golden.
//
// Each diskless machine has its own target "<golden iqn>.m<mac>" (mac = 12 hex digits): the golden at the version the
// machine was prepared on, then its base layer (the machine's own first-boot setup: name, licence — kept while the
// golden version stays the same), then its session layer (everything it writes until mgmt prepares its next boot:
// dropped then, or promoted to the base when that boot was the first one). Only that machine — its IP and the MAC the
// kernel sees for it (ARP) — may log in to it. Its layers live in the golden's machine folder (on the writeback volume):
//   <dir>/<mac>/base.ovl, base.blocks, base.gen      the base layer and the version it was built on
//   <dir>/<mac>/session.ovl, session.blocks, session.gen
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use serde::{Deserialize, Serialize};

use super::Lun;
use super::games::{self, Registry, View};
use super::overlay::Overlay;

/// The goldens.
pub static GOLDENS: Registry = Registry::new("iscsid-goldens.json", "golden");

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Machine {
    /// Any usual spelling (aa:bb:…, AA-BB-…, aabb…).
    pub mac: String,
    /// Its fixed IP: the only address its target answers.
    pub ip: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Config {
    /// The golden as a layered disk: name, target prefix, golden.img, maintenance machine (update_ip / update_mac),
    /// RAM option.
    pub disk: games::Config,
    /// Folder of its machines' layers.
    pub dir: String,
    /// The diskless machines that boot it.
    #[serde(default)]
    pub machines: Vec<Machine>,
}

/// What `prepare` set up.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Prepared {
    /// The golden version this boot reads.
    pub ver: u64,
    /// No base layer: this boot is the machine's first on this version (its setup runs, then `promote`).
    pub first: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct MachineInfo {
    pub mac: String,
    pub ip: String,
    /// Version its base layer was built on (None = no base).
    pub base_ver: Option<u64>,
    pub base_bytes: u64,
    /// Version its current session reads (None = not prepared).
    pub session_ver: Option<u64>,
    pub session_bytes: u64,
    pub connected: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Info {
    pub disk: games::Info,
    pub machines: Vec<MachineInfo>,
}

/// A machine's open layers, shared by its sessions (reconnects).
#[derive(Default)]
struct Mach {
    base: Option<Arc<Overlay>>,
    session: Option<Arc<Overlay>>,
    users: usize,
}

static CFGS: Mutex<Vec<Config>> = Mutex::new(Vec::new());
/// By (golden, mac).
static MACHS: LazyLock<Mutex<HashMap<(String, String), Mach>>> = LazyLock::new(Default::default);

/// "AA:BB:CC:DD:EE:FF" / "aa-bb-…" / "aabbccddeeff" → "aabbccddeeff". None = not a MAC.
pub fn mac12(mac: &str) -> Option<String> {
    let m: String = mac
        .chars()
        .filter(|c| !matches!(c, ':' | '-' | '.'))
        .collect::<String>()
        .to_ascii_lowercase();
    (m.len() == 12 && m.chars().all(|c| c.is_ascii_hexdigit())).then_some(m)
}

/// Target name of machine `mac` on the golden whose prefix is `iqn`.
pub fn machine_iqn(iqn: &str, mac: &str) -> Option<String> {
    mac12(mac).map(|m| format!("{iqn}.m{m}"))
}

fn files(dir: &Path, what: &str) -> (PathBuf, PathBuf, PathBuf) {
    (
        dir.join(format!("{what}.ovl")),
        dir.join(format!("{what}.blocks")),
        dir.join(format!("{what}.gen")),
    )
}

fn read_gen(p: &Path) -> Option<u64> {
    std::fs::read_to_string(p).ok()?.trim().parse().ok()
}

fn remove(dir: &Path, what: &str) {
    let (o, b, g) = files(dir, what);
    for p in [g, b, o] {
        let _ = std::fs::remove_file(p);
    }
}

/// Bytes a layer holds, from its block list (4 bytes per 64 KB block written).
fn listed_bytes(blocks: &Path) -> u64 {
    std::fs::metadata(blocks).map_or(0, |m| m.len() / 4 * super::overlay::BLOCK)
}

/// The golden and machine a target name is for: (config, mac).
fn target(iqn: &str) -> Option<(Config, String)> {
    CFGS.lock().iter().find_map(|c| {
        let m = mac12(iqn.strip_prefix(&c.disk.iqn)?.strip_prefix(".m")?)?;
        c.machines
            .iter()
            .any(|x| mac12(&x.mac).as_deref() == Some(m.as_str()))
            .then(|| (c.clone(), m))
    })
}

fn config(name: &str) -> Result<Config, String> {
    CFGS.lock()
        .iter()
        .find(|c| c.disk.name == name)
        .cloned()
        .ok_or_else(|| format!("golden {name} not served"))
}

fn machine_dir(c: &Config, mac: &str) -> PathBuf {
    Path::new(&c.dir).join(mac)
}

/// The kept configs (next to the daemon's state file).
fn cfg_file() -> Option<PathBuf> {
    super::state_path().map(|p| p.with_file_name("iscsid-diskless.json"))
}

/// Serve exactly the goldens `cfgs`. A machine no longer listed loses its open layers (its files stay: `forget`).
pub fn set(cfgs: Vec<Config>) -> Result<(), String> {
    for c in &cfgs {
        for m in &c.machines {
            mac12(&m.mac).ok_or_else(|| format!("golden {}: bad MAC {}", c.disk.name, m.mac))?;
        }
    }
    *CFGS.lock() = cfgs.clone();
    MACHS.lock().retain(|(g, mac), m| {
        m.users > 0
            || cfgs.iter().any(|c| {
                &c.disk.name == g
                    && c.machines
                        .iter()
                        .any(|x| mac12(&x.mac).as_deref() == Some(mac.as_str()))
            })
    });
    if let Some(p) = cfg_file() {
        let _ = std::fs::write(p, serde_json::to_vec(&cfgs).unwrap());
    }
    GOLDENS.set(cfgs.into_iter().map(|c| c.disk).collect())
}

/// At daemon start: the configs of the previous run.
pub fn restore() {
    let Some(cfgs) = cfg_file()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice::<Vec<Config>>(&b).ok())
    else {
        return;
    };
    if let Err(e) = set(cfgs) {
        tracing::error!("{e}");
    }
}

/// A diskless machine's target name.
pub fn owns(iqn: &str) -> bool {
    target(iqn).is_some()
}

/// The golden's file, for a machine's target.
pub fn lun(iqn: &str) -> Option<Arc<Lun>> {
    let (c, _) = target(iqn)?;
    GOLDENS.lun(&format!(
        "{}.g{}",
        c.disk.iqn,
        GOLDENS.newest(&c.disk.name)?
    ))
}

/// `peer` is machine `mac`'s IP, and the kernel sees `mac` behind it.
pub(crate) fn machine_allowed(
    m: &Machine,
    mac: &str,
    peer: &str,
    arp: impl FnOnce() -> Option<String>,
) -> bool {
    m.ip == peer && arp().and_then(|a| mac12(&a)).as_deref() == Some(mac)
}

/// A session of machine target `iqn` from `peer` logs in: golden (at the prepared version) ← base ← session (writable).
pub fn attach(iqn: &str, peer: &str) -> Result<View, String> {
    let (c, mac) = target(iqn).ok_or("not a diskless machine target")?;
    let m = c
        .machines
        .iter()
        .find(|x| mac12(&x.mac).as_deref() == Some(mac.as_str()))
        .ok_or("machine not listed")?;
    if !machine_allowed(m, &mac, peer, || super::arp_mac(peer)) {
        tracing::warn!(
            "golden {}: {peer} tried machine {mac}'s disk (it is at {}) — refused",
            c.disk.name,
            m.ip
        );
        return Err("only that machine may use its disk".into());
    }
    let dir = machine_dir(&c, &mac);
    let (s_ovl, s_list, s_gen) = files(&dir, "session");
    let ver = read_gen(&s_gen).ok_or("not prepared for a boot — PXE boot it")?;
    let below = GOLDENS
        .count(&c.disk.name, ver)
        .ok_or("its golden version is no longer served — PXE boot it again")?;
    let size = below.lun.size;
    let r = (|| -> Result<View, String> {
        let mut layers = below.layers;
        let mut all = MACHS.lock();
        let st = all.entry((c.disk.name.clone(), mac.clone())).or_default();
        let (b_ovl, b_list, b_gen) = files(&dir, "base");
        if st.base.is_none() && read_gen(&b_gen) == Some(ver) && b_ovl.exists() {
            st.base = Some(Arc::new(Overlay::open(&b_ovl, &b_list, size)?));
        }
        if let Some(b) = &st.base {
            layers.push(b.clone());
        }
        if st.session.is_none() {
            st.session = Some(Arc::new(Overlay::open(&s_ovl, &s_list, size)?));
        }
        st.users += 1;
        Ok(View {
            disk: c.disk.name.clone(),
            ver,
            layers,
            ov: st.session.clone(),
            reg: &GOLDENS,
            machine: Some(mac.clone()),
        })
    })();
    match &r {
        Ok(_) => tracing::info!("golden {}: machine {mac} ({peer}) on g{ver}", c.disk.name),
        Err(_) => GOLDENS.uncount(&c.disk.name, ver),
    }
    r
}

pub fn detach(v: &View) {
    GOLDENS.uncount(&v.disk, v.ver);
    let Some(mac) = &v.machine else { return };
    if let Some(st) = MACHS.lock().get_mut(&(v.disk.clone(), mac.clone())) {
        st.users = st.users.saturating_sub(1);
    }
    // Outside the lock: an fsync.
    if let Some(ov) = &v.ov
        && let Err(e) = ov.flush()
    {
        tracing::error!("golden {} machine {mac}: {e}", v.disk);
    }
}

/// Machine `mac` is about to boot golden `name` (mgmt, at its PXE boot; its earlier sessions must be gone — KickPeer
/// first). Its session layer is dropped — or, `promote`, becomes its base layer (the first boot's setup is done) when
/// it was on the version served now. Its base layer is dropped with `drop_base` (new name, new licence…) or when it
/// was built on another version. Then the new session reads the newest version.
pub fn prepare(name: &str, mac: &str, promote: bool, drop_base: bool) -> Result<Prepared, String> {
    let c = config(name)?;
    let mac = mac12(mac).ok_or_else(|| format!("bad MAC {mac}"))?;
    if !c
        .machines
        .iter()
        .any(|x| mac12(&x.mac).as_deref() == Some(mac.as_str()))
    {
        return Err(format!("machine {mac} does not boot golden {name}"));
    }
    let ver = GOLDENS
        .newest(name)
        .ok_or_else(|| format!("golden {name} not served"))?;
    let key = (name.to_string(), mac.clone());
    let old = {
        let mut all = MACHS.lock();
        if all.get(&key).is_some_and(|m| m.users > 0) {
            return Err(format!(
                "machine {mac} is still connected to golden {name} — it must be off or in its boot menu"
            ));
        }
        all.remove(&key)
    };
    if let Some(s) = old.as_ref().and_then(|m| m.session.as_ref()) {
        s.flush()?;
    }
    drop(old);
    let dir = machine_dir(&c, &mac);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let (s_ovl, s_list, s_gen) = files(&dir, "session");
    let (b_ovl, b_list, b_gen) = files(&dir, "base");
    if promote && read_gen(&s_gen) == Some(ver) && s_ovl.exists() {
        // The base's version file last: a promotion cut off midway leaves a base without one → dropped below.
        remove(&dir, "base");
        let r = std::fs::rename(&s_ovl, &b_ovl).and_then(|_| {
            if s_list.exists() {
                std::fs::rename(&s_list, &b_list)
            } else {
                Ok(())
            }
        });
        r.map_err(|e| format!("{}: {e}", dir.display()))?;
        crate::versions::write_durable(&b_gen, ver.to_string().as_bytes())
            .map_err(|e| format!("{}: {e}", b_gen.display()))?;
        tracing::info!("golden {name}: machine {mac}'s first boot on g{ver} kept as its base");
    }
    if drop_base || read_gen(&b_gen) != Some(ver) || !b_ovl.exists() {
        remove(&dir, "base");
    }
    remove(&dir, "session");
    crate::versions::write_durable(&s_gen, ver.to_string().as_bytes())
        .map_err(|e| format!("{}: {e}", s_gen.display()))?;
    Ok(Prepared {
        ver,
        first: !b_ovl.exists(),
    })
}

/// Delete machine `mac`'s layers of golden `name` (it is no longer diskless, or forgotten). It must be off.
pub fn forget(name: &str, mac: &str) -> Result<(), String> {
    let c = config(name)?;
    let mac = mac12(mac).ok_or_else(|| format!("bad MAC {mac}"))?;
    {
        let mut all = MACHS.lock();
        let key = (name.to_string(), mac.clone());
        if all.get(&key).is_some_and(|m| m.users > 0) {
            return Err(format!(
                "machine {mac} is still connected — shut it down first"
            ));
        }
        all.remove(&key);
    }
    let dir = machine_dir(&c, &mac);
    match std::fs::remove_dir_all(&dir) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(format!("{}: {e}", dir.display()))
        }
        _ => Ok(()),
    }
}

pub fn info() -> Vec<Info> {
    let disks = GOLDENS.info();
    let cfgs = CFGS.lock().clone();
    let all = MACHS.lock();
    cfgs.iter()
        .map(|c| Info {
            disk: disks
                .iter()
                .find(|d| d.name == c.disk.name)
                .cloned()
                .unwrap_or_default(),
            machines: c
                .machines
                .iter()
                .filter_map(|m| {
                    let mac = mac12(&m.mac)?;
                    let dir = machine_dir(c, &mac);
                    let st = all.get(&(c.disk.name.clone(), mac.clone()));
                    let (_, b_list, b_gen) = files(&dir, "base");
                    let (_, s_list, s_gen) = files(&dir, "session");
                    Some(MachineInfo {
                        ip: m.ip.clone(),
                        base_ver: read_gen(&b_gen),
                        base_bytes: st
                            .and_then(|s| s.base.as_ref())
                            .map_or_else(|| listed_bytes(&b_list), |o| o.used()),
                        session_ver: read_gen(&s_gen),
                        session_bytes: st
                            .and_then(|s| s.session.as_ref())
                            .map_or_else(|| listed_bytes(&s_list), |o| o.used()),
                        connected: st.is_some_and(|s| s.users > 0),
                        mac,
                    })
                })
                .collect(),
        })
        .collect()
}

/// Tests that set the goldens run one at a time (the list is the daemon's, one per process).
#[cfg(test)]
pub(crate) static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(test)]
#[path = "../../tests/unit/iscsid/golden.rs"]
mod tests;
