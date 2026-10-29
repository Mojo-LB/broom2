// iscsid/games.rs — the games disks: shared disks served to the Windows clients READ-ONLY (which machine gets which
// disk is mgmt's business, games.rs there); each client puts a differencing VHDX on its own SSD over the games.vhdx
// inside a disk, so its writes stay local and go at reboot (broom-games.ps1).
//
// Versions ("generations"), per disk: gen k = games.img + the frozen layers 1..=k on top (layer-<k>/, the blocks that
// update changed, overlay.rs), each served as target "<iqn>.g<k>". The disk's update machine (by IP, set by mgmt) gets
// the current gen writable, its writes in the update overlay; "save" freezes that into the next layer = a new gen at
// once. Clients connect to the newest gen when they boot; a client already playing keeps its gen until it reboots.
// Once no client has used an older gen for a while, the layers are merged into games.img in the background — safe
// while clients of the newest gen read: merging only writes blocks a layer covers, and they read those from the layer.
//
// RAM option, per disk: games.img copied into RAM (ramimg.rs) in the background, served from the file meanwhile. The
// copy stays right while layers sit on top of it (a merge only rewrites blocks the layers cover); once a merge drops
// those layers the disk goes back to the file at once and the copy is loaded again.
//
// The same engine (a Registry: its own list of disks and its own config file) also serves the diskless Windows
// goldens (golden.rs): a golden is such a disk whose update machine is the golden's maintenance machine.
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::Lun;
use super::overlay::Overlay;

/// An older gen nobody used for this long is dropped (its layers merged): a client that lost its connection reconnects
/// within it.
const GRACE: Duration = Duration::from_secs(600);

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Config {
    /// The disk's name (mgmt's list).
    pub name: String,
    /// Target name prefix: gen k is "<iqn>.g<k>".
    pub iqn: String,
    /// Its games.img; its folder holds the layers and the update.
    pub path: String,
    /// IP of the disk's update machine (None = no update running).
    #[serde(default)]
    pub update_ip: Option<String>,
    /// Its MAC: the update session needs both (an IP alone is set by anyone on the LAN). None = IP only.
    #[serde(default)]
    pub update_mac: Option<String>,
    /// Serve games.img from a copy in RAM.
    #[serde(default)]
    pub ram: bool,
    /// RAM bytes that must stay free next to the copy (not loaded otherwise).
    #[serde(default)]
    pub reserve: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Info {
    pub name: String,
    /// The gen clients get when they boot.
    pub ver: u64,
    pub sessions: usize,
    /// Sessions still on an older gen (clients that have not rebooted since the last save).
    pub old_sessions: usize,
    /// Layers not merged into games.img yet, and their size.
    pub layers: usize,
    pub layer_bytes: u64,
    /// The update not saved yet.
    pub update_bytes: u64,
    pub update_connected: bool,
    pub merging: bool,
    /// Bytes read by clients since the disk was set up (benchmark page). 0 from a daemon of a build without it.
    #[serde(default)]
    pub read_bytes: u64,
    /// Served from its RAM copy now / the copy is being loaded.
    #[serde(default)]
    pub ram: bool,
    #[serde(default)]
    pub ram_loading: bool,
}

/// What a session of one gen reads: games.img under `layers` (oldest first); `ov` = its writes (the update machine).
pub struct View {
    pub disk: String,
    pub ver: u64,
    pub layers: Vec<Arc<Overlay>>,
    pub ov: Option<Arc<Overlay>>,
    /// The family the disk belongs to: the session detaches from it.
    pub reg: &'static Registry,
    /// A diskless machine's own session (golden.rs: its MAC), not a session of the shared disk.
    pub machine: Option<String>,
}

struct Layer {
    ver: u64,
    ov: Arc<Overlay>,
}

struct State {
    cfg: Config,
    lun: Arc<Lun>,
    dir: PathBuf,
    /// games.img holds gens up to this one.
    base_ver: u64,
    ver: u64,
    layers: Vec<Layer>,
    sessions: HashMap<u64, usize>,
    /// Since when no session uses an older gen.
    old_idle: Instant,
    update: Option<Arc<Overlay>>,
    update_users: usize,
    /// Merging the layers up to this gen: older gens are no longer served.
    merging: Option<u64>,
    /// Merges done: a RAM copy loaded across one is stale.
    merges: u64,
    ram_loading: bool,
}

/// One family of layered disks: its disks by name, the file keeping their configs for the daemon's next start (next to
/// its state file), and the word naming one in the log.
pub struct Registry {
    disks: Mutex<Option<HashMap<String, State>>>,
    cfg_name: &'static str,
    what: &'static str,
}

/// The games disks.
pub static GAMES: Registry = Registry::new("iscsid-games.json", "iSCSI disk");

fn update_files(dir: &Path) -> (PathBuf, PathBuf) {
    (dir.join("update.ovl"), dir.join("update.blocks"))
}

fn layer_dir(dir: &Path, ver: u64) -> PathBuf {
    dir.join(format!("layer-{ver}"))
}

fn open_layer(dir: &Path, ver: u64, size: u64) -> Result<Overlay, String> {
    let d = layer_dir(dir, ver);
    Overlay::open(&d.join("ovl"), &d.join("blocks"), size)
}

fn open_lun(cfg: &Config) -> Result<Arc<Lun>, String> {
    Ok(Arc::new(Lun::file(&cfg.iqn, &cfg.path)?))
}

/// The disk's games.img now served from `data` (sessions already in keep theirs); the read counter goes on.
fn swap_lun(s: &mut State, data: super::Data) {
    let read = s.lun.read_bytes.load(std::sync::atomic::Ordering::Relaxed);
    s.lun = Arc::new(Lun::new(s.cfg.iqn.clone(), s.lun.size, data, read));
}

/// Back to reading games.img (RAM option off, or the copy went stale).
fn to_file(s: &mut State) -> Result<(), String> {
    if s.lun.is_ram() {
        let f = std::fs::File::open(&s.cfg.path).map_err(|e| format!("{}: {e}", s.cfg.path))?;
        swap_lun(s, super::Data::File(f));
    }
    Ok(())
}

/// Blocking read of gen view `layers` (oldest first) over games.img.
pub fn read_chain(
    layers: &[Arc<Overlay>],
    lun: &Lun,
    off: u64,
    buf: &mut [u8],
) -> Result<(), String> {
    match layers.split_last() {
        None => lun.read_at(off, buf),
        Some((top, rest)) => top.read_at(&|o, b| read_chain(rest, lun, o, b), off, buf),
    }
}

/// The state of `dir` on disk: games.img's gen, the layers above it. Leftovers of a crash are cleaned up.
fn load(dir: &Path, size: u64) -> Result<(u64, Vec<Layer>), String> {
    let base_ver: u64 = std::fs::read_to_string(dir.join("base.gen"))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    let mut layers = Vec::new();
    for e in std::fs::read_dir(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .flatten()
    {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(n) = name.strip_prefix("layer-") else {
            continue;
        };
        match n.parse::<u64>() {
            Ok(g) if g > base_ver => layers.push(Layer {
                ver: g,
                ov: Arc::new(open_layer(dir, g, size)?),
            }),
            _ => {
                let _ = std::fs::remove_dir_all(e.path()); // merged already, or a save cut off (.tmp)
            }
        }
    }
    layers.sort_by_key(|l| l.ver);
    if layers.is_empty() {
        let _ = std::fs::remove_file(dir.join("merging")); // a merge that ended right before removing it
    }
    // A save cut off after the layer was in place: the update files are the same files (hard links) → gone.
    use std::os::unix::fs::MetadataExt;
    let (ovl, list) = update_files(dir);
    if let (Some(top), Ok(u)) = (layers.last(), std::fs::metadata(&ovl))
        && std::fs::metadata(layer_dir(dir, top.ver).join("ovl")).is_ok_and(|m| m.ino() == u.ino())
    {
        let _ = std::fs::remove_file(&ovl);
        let _ = std::fs::remove_file(&list);
    }
    Ok((base_ver, layers))
}

/// The disk and gen `iqn` names, if it is a gen still served.
fn find<'a>(d: &'a mut HashMap<String, State>, iqn: &str) -> Option<(&'a mut State, u64)> {
    d.values_mut().find_map(|s| {
        let k: u64 = iqn
            .strip_prefix(&s.cfg.iqn)?
            .strip_prefix(".g")?
            .parse()
            .ok()?;
        served(s, k).then_some((s, k))
    })
}

/// Gen `k` of the disk can still be read (not merged away, not in the future).
fn served(s: &State, k: u64) -> bool {
    k >= s.merging.unwrap_or(s.base_ver) && k <= s.ver
}

/// `peer` is the disk's update machine: its IP, and its MAC as the kernel sees it (ARP) when the disk names one — an
/// IP alone is taken by anyone who sets it while the update machine is off, and its writes would reach every machine
/// at the next Save. Another MAC on that IP gets a read-only session (logged).
pub(crate) fn update_allowed(
    cfg: &Config,
    peer: &str,
    arp: impl FnOnce() -> Option<String>,
    name: &str,
) -> bool {
    if cfg.update_ip.as_deref() != Some(peer) {
        return false;
    }
    let Some(want) = &cfg.update_mac else {
        return true;
    };
    let got = arp();
    if got.as_deref().is_some_and(|m| m.eq_ignore_ascii_case(want)) {
        return true;
    }
    tracing::warn!(
        "{name}: {peer} is the update machine's IP but its MAC is {} (expected {want}) — read-only",
        got.as_deref().unwrap_or("unknown")
    );
    false
}

fn old_sessions(s: &State) -> usize {
    s.sessions
        .iter()
        .filter(|(g, _)| **g < s.ver)
        .map(|(_, n)| n)
        .sum()
}

/// What a diskless machine's session reads below its own writes (golden.rs): the disk's file, the frozen layers up to
/// its gen. Counted as a session of that gen (merges wait for it) until `uncount`.
pub(super) struct Below {
    pub lun: Arc<Lun>,
    pub layers: Vec<Arc<Overlay>>,
}

impl Registry {
    pub const fn new(cfg_name: &'static str, what: &'static str) -> Registry {
        Registry {
            disks: Mutex::new(None),
            cfg_name,
            what,
        }
    }

    fn with<T>(&self, f: impl FnOnce(&mut HashMap<String, State>) -> T) -> T {
        f(self.disks.lock().get_or_insert_with(HashMap::new))
    }

    /// Where the configs are kept for the daemon's next start (next to its state file).
    fn cfg_file(&self) -> Option<PathBuf> {
        super::state_path().map(|p| p.with_file_name(self.cfg_name))
    }

    /// Disk `name` set to RAM and not in RAM yet → load the copy on a thread, swapped in when done unless games.img
    /// changed meanwhile (a merge, the disk grew → loaded again). Not enough RAM → stays on the file.
    fn load_ram(&'static self, name: &str) {
        let Some((path, reserve, merges)) = self.with(|d| {
            let s = d.get_mut(name)?;
            if !s.cfg.ram || s.ram_loading || s.lun.is_ram() {
                return None;
            }
            s.ram_loading = true;
            Some((s.cfg.path.clone(), s.cfg.reserve, s.merges))
        }) else {
            return;
        };
        let name = name.to_string();
        let what = self.what;
        std::thread::spawn(move || {
            let r = super::ramimg::RamImage::load(&path, reserve);
            let (again, unused) = self.with(|d| {
                let Some(s) = d.get_mut(&name) else {
                    return (false, r.ok());
                };
                s.ram_loading = false;
                match r {
                    Ok(img)
                        if s.cfg.ram
                            && s.cfg.path == path
                            && s.merges == merges
                            && s.lun.size == img.size() =>
                    {
                        swap_lun(s, super::Data::Ram(img));
                        tracing::info!("{what} {name}: served from RAM");
                        (false, None)
                    }
                    Ok(img) => (s.cfg.ram, Some(img)),
                    Err(e) => {
                        tracing::warn!(
                            "{what} {name}: not loaded into RAM ({e}) → served from the file"
                        );
                        (false, None)
                    }
                }
            });
            drop(unused); // outside the lock: unmapping a big copy takes a while
            if again {
                self.load_ram(&name);
            }
        });
    }

    /// Serve exactly the disks `cfgs` (others stop being served; their connected clients stay connected). Each disk is
    /// set up on its own: one that fails doesn't keep the others from being served.
    pub fn set(&'static self, cfgs: Vec<Config>) -> Result<(), String> {
        let mut errs = Vec::new();
        self.with(|d| d.retain(|name, _| cfgs.iter().any(|c| &c.name == name)));
        for c in &cfgs {
            if let Err(e) = self.set_one(c.clone()) {
                errs.push(format!("{} {}: {e}", self.what, c.name));
            }
        }
        if let Some(p) = self.cfg_file() {
            let _ = std::fs::write(p, serde_json::to_vec(&cfgs).unwrap());
        }
        if errs.is_empty() {
            Ok(())
        } else {
            Err(errs.join("; "))
        }
    }

    fn set_one(&'static self, cfg: Config) -> Result<(), String> {
        let dir = Path::new(&cfg.path)
            .parent()
            .ok_or("path has no folder")?
            .to_path_buf();
        let size = std::fs::metadata(&cfg.path)
            .map_err(|e| format!("{}: {e}", cfg.path))?
            .len();
        let name = cfg.name.clone();
        self.with(|d| -> Result<(), String> {
            match d.get_mut(&name) {
                Some(s) if s.cfg.path == cfg.path => {
                    if s.lun.size != size {
                        if s.update_users > 0 {
                            return Err(
                                "the update machine is connected — shut it down first".into()
                            );
                        }
                        // The disk grew: sessions from now on see the new size; the update overlay grows on reopen.
                        s.lun = open_lun(&cfg)?;
                        s.update = None;
                    }
                    s.cfg = cfg;
                    if !s.cfg.ram {
                        to_file(s)?;
                    }
                }
                _ => {
                    let lun = open_lun(&cfg)?;
                    let (base_ver, layers) = load(&dir, size)?;
                    let ver = layers.last().map_or(base_ver, |l| l.ver);
                    let s = State {
                        cfg,
                        lun,
                        dir: dir.clone(),
                        base_ver,
                        ver,
                        layers,
                        sessions: HashMap::new(),
                        old_idle: Instant::now(),
                        update: None,
                        update_users: 0,
                        merging: None,
                        merges: 0,
                        ram_loading: false,
                    };
                    d.insert(name.clone(), s);
                }
            }
            Ok(())
        })?;
        self.load_ram(&name);
        // A merge cut off by a crash / restart: games.img is half merged, only the newest gen reads it right → finish.
        if dir.join("merging").exists() {
            tracing::warn!("{} {name}: finishing a merge cut off earlier", self.what);
            self.merge_if_idle(&name, Duration::ZERO);
        }
        Ok(())
    }

    /// At daemon start: the configs of the previous run.
    pub fn restore(&'static self) {
        let Some(cfgs) = self
            .cfg_file()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice::<Vec<Config>>(&b).ok())
        else {
            return;
        };
        if let Err(e) = self.set(cfgs) {
            tracing::error!("{e}");
        }
    }

    /// The disk file if `iqn` names a gen still served.
    pub fn lun(&self, iqn: &str) -> Option<Arc<Lun>> {
        self.with(|d| find(d, iqn).map(|(s, _)| s.lun.clone()))
    }

    /// Target names of every disk's newest gen (discovery).
    pub fn iqns(&self) -> Vec<String> {
        self.with(|d| {
            d.values()
                .map(|s| format!("{}.g{}", s.cfg.iqn, s.ver))
                .collect()
        })
    }

    /// A target name of one of the disks (any gen): its sessions attach / detach.
    pub fn owns(&self, iqn: &str) -> bool {
        self.with(|d| {
            d.values().any(|s| {
                iqn.strip_prefix(&s.cfg.iqn)
                    .is_some_and(|r| r.starts_with(".g"))
            })
        })
    }

    /// A session of `iqn` from `peer` logs in. The disk's update machine on the newest gen gets the update overlay
    /// (writable). Every Ok is paired with a `detach`.
    pub fn attach(&'static self, iqn: &str, peer: &str) -> Result<View, String> {
        self.with(|d| {
            let (s, ver) = find(d, iqn).ok_or("version no longer served")?;
            let layers = s
                .layers
                .iter()
                .filter(|l| l.ver <= ver)
                .map(|l| l.ov.clone())
                .collect();
            let mut ov = None;
            let label = format!("{} {}", self.what, s.cfg.name);
            let writer =
                ver == s.ver && update_allowed(&s.cfg, peer, || super::arp_mac(peer), &label);
            if writer {
                if s.update.is_none() {
                    let (ovl, list) = update_files(&s.dir);
                    s.update = Some(Arc::new(Overlay::open(&ovl, &list, s.lun.size)?));
                }
                s.update_users += 1;
                ov = s.update.clone();
                tracing::info!(
                    "{label}: {peer} in UPDATE mode on g{ver} — its writes are kept for saving"
                );
            }
            *s.sessions.entry(ver).or_default() += 1;
            Ok(View {
                disk: s.cfg.name.clone(),
                ver,
                layers,
                ov,
                reg: self,
                machine: None,
            })
        })
    }

    pub fn detach(&self, v: &View) {
        self.uncount(&v.disk, v.ver);
        self.with(|d| {
            let Some(s) = d.get_mut(&v.disk) else { return };
            if let Some(ov) = &v.ov
                && s.update.as_ref().is_some_and(|u| Arc::ptr_eq(u, ov))
            {
                s.update_users = s.update_users.saturating_sub(1);
            }
        });
        // Outside the lock: an fsync.
        if let Some(ov) = &v.ov
            && let Err(e) = ov.flush()
        {
            tracing::error!("{} {} update: {e}", self.what, v.disk);
        }
    }

    /// The disk's newest gen (None = not served).
    pub(super) fn newest(&self, name: &str) -> Option<u64> {
        self.with(|d| d.get(name).map(|s| s.ver))
    }

    /// What gen `ver` of disk `name` reads (None = not served any more), counted as one of its sessions.
    pub(super) fn count(&self, name: &str, ver: u64) -> Option<Below> {
        self.with(|d| {
            let s = d.get_mut(name)?;
            if !served(s, ver) {
                return None;
            }
            *s.sessions.entry(ver).or_default() += 1;
            Some(Below {
                lun: s.lun.clone(),
                layers: s
                    .layers
                    .iter()
                    .filter(|l| l.ver <= ver)
                    .map(|l| l.ov.clone())
                    .collect(),
            })
        })
    }

    /// The end of a session counted by `attach` / `count`.
    pub(super) fn uncount(&self, name: &str, ver: u64) {
        self.with(|d| {
            let Some(s) = d.get_mut(name) else { return };
            if let Some(n) = s.sessions.get_mut(&ver) {
                *n -= 1;
                if *n == 0 {
                    s.sessions.remove(&ver);
                    if ver < s.ver && old_sessions(s) == 0 {
                        s.old_idle = Instant::now();
                    }
                }
            }
        });
    }

    /// Every 30 s: merge each disk's layers into its file once no session has used an older gen for GRACE. Blocking.
    pub fn tick(&'static self) {
        for name in self.with(|d| d.keys().cloned().collect::<Vec<_>>()) {
            self.merge_if_idle(&name, GRACE);
        }
    }

    pub fn merge_if_idle(&'static self, name: &str, grace: Duration) {
        let job = self.with(|d| {
            let s = d.get_mut(name)?;
            if s.layers.is_empty()
                || s.merging.is_some()
                || old_sessions(s) > 0
                || s.old_idle.elapsed() < grace
            {
                return None;
            }
            let target = s.ver;
            s.merging = Some(target); // older gens are not served from now on
            let layers: Vec<(u64, Arc<Overlay>)> =
                s.layers.iter().map(|l| (l.ver, l.ov.clone())).collect();
            Some((target, layers, s.cfg.path.clone(), s.dir.clone()))
        });
        let Some((target, layers, path, dir)) = job else {
            return;
        };
        let what = self.what;
        let marker = dir.join("merging");
        let r = (|| -> Result<(), String> {
            std::fs::write(&marker, target.to_string())
                .map_err(|e| format!("{}: {e}", marker.display()))?;
            let f = std::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .map_err(|e| format!("{path}: {e}"))?;
            for (_, l) in &layers {
                l.merge_into(&f)?; // oldest first: a newer layer's block wins
            }
            crate::versions::write_durable(&dir.join("base.gen"), target.to_string().as_bytes())
                .map_err(|e| format!("base.gen: {e}"))
        })();
        self.with(|d| {
            let Some(s) = d.get_mut(name) else { return };
            s.merging = None;
            match r {
                Ok(()) => {
                    // Sessions holding these layers keep reading them (open files) — same bytes as the file now.
                    // A RAM copy holds the old bytes under those layers: new sessions read the file until it is loaded
                    // again.
                    s.merges += 1;
                    if let Err(e) = to_file(s) {
                        tracing::error!("{what} {name}: {e}");
                        return; // layers kept: still served right from the stale copy; retried at the next tick
                    }
                    s.base_ver = target;
                    s.layers.retain(|l| l.ver > target);
                    for (g, _) in &layers {
                        let _ = std::fs::remove_dir_all(layer_dir(&dir, *g));
                    }
                    let _ = std::fs::remove_file(&marker);
                    tracing::info!("{what} {name}: versions up to g{target} merged into {path}");
                }
                Err(e) => tracing::error!("{what} {name}: merge failed (retried later): {e}"),
            }
        });
        self.load_ram(name);
    }

    /// Freeze the update into a new gen, served at once (machines get it at their next boot). The update machine must
    /// be off: its file system must be on disk, whole.
    pub fn save(&self, name: &str) -> Result<String, String> {
        let what = self.what;
        self.with(|d| {
            let s = d.get_mut(name).ok_or(format!("{what} not served"))?;
            if s.update_users > 0 {
                return Err(
                    "the update machine is still connected — shut it down properly first".into(),
                );
            }
            let (ovl, list) = update_files(&s.dir);
            let ov = match s.update.take() {
                Some(o) => o,
                None if ovl.exists() => Arc::new(Overlay::open(&ovl, &list, s.lun.size)?),
                None => return Err("no update to save".into()),
            };
            ov.flush()?;
            if ov.used() == 0 {
                return Err("the update is empty".into());
            }
            let ver = s.ver + 1;
            // Hard links into layer-<ver>.tmp, then one rename: the layer appears whole or not at all.
            let (tmp, dst) = (
                s.dir.join(format!("layer-{ver}.tmp")),
                layer_dir(&s.dir, ver),
            );
            let _ = std::fs::remove_dir_all(&tmp);
            let r = std::fs::create_dir(&tmp)
                .and_then(|_| std::fs::hard_link(&ovl, tmp.join("ovl")))
                .and_then(|_| std::fs::hard_link(&list, tmp.join("blocks")))
                .and_then(|_| std::fs::rename(&tmp, &dst));
            if let Err(e) = r {
                let _ = std::fs::remove_dir_all(&tmp);
                s.update = Some(ov);
                return Err(format!("{}: {e}", dst.display()));
            }
            drop(ov);
            let _ = std::fs::remove_file(&ovl);
            let _ = std::fs::remove_file(&list);
            let layer = open_layer(&s.dir, ver, s.lun.size)?;
            s.layers.push(Layer {
                ver,
                ov: Arc::new(layer),
            });
            s.ver = ver;
            if old_sessions(s) == 0 {
                s.old_idle = Instant::now();
            }
            tracing::info!("{what} {name}: version g{ver} ready");
            Ok(format!(
                "version g{ver} ready: each machine gets it at its next boot"
            ))
        })
    }

    /// Throw the update away (the update machine must be off).
    pub fn discard(&self, name: &str) -> Result<(), String> {
        let what = self.what;
        self.with(|d| {
            let s = d.get_mut(name).ok_or(format!("{what} not served"))?;
            if s.update_users > 0 {
                return Err("the update machine is still connected — shut it down first".into());
            }
            let (ovl, list) = update_files(&s.dir);
            drop(s.update.take());
            let _ = std::fs::remove_file(ovl);
            let _ = std::fs::remove_file(list);
            Ok(())
        })
    }

    pub fn info(&self) -> Vec<Info> {
        self.with(|d| {
            d.values()
                .map(|s| Info {
                    name: s.cfg.name.clone(),
                    ver: s.ver,
                    sessions: s.sessions.values().sum(),
                    old_sessions: old_sessions(s),
                    layers: s.layers.len(),
                    layer_bytes: s.layers.iter().map(|l| l.ov.used()).sum(),
                    update_bytes: match &s.update {
                        Some(u) => u.used(),
                        None => std::fs::metadata(update_files(&s.dir).1)
                            .map_or(0, |m| m.len() / 4 * super::overlay::BLOCK),
                    },
                    update_connected: s.update_users > 0,
                    merging: s.merging.is_some(),
                    read_bytes: s.lun.read_bytes.load(std::sync::atomic::Ordering::Relaxed),
                    ram: s.lun.is_ram(),
                    ram_loading: s.ram_loading,
                })
                .collect()
        })
    }
}

// The games disks' entry points (mod.rs, conn.rs, the tests).

pub fn set(cfgs: Vec<Config>) -> Result<(), String> {
    GAMES.set(cfgs)
}

pub fn restore() {
    GAMES.restore()
}

pub fn lun(iqn: &str) -> Option<Arc<Lun>> {
    GAMES.lun(iqn)
}

pub fn iqns() -> Vec<String> {
    GAMES.iqns()
}

pub fn tick() {
    GAMES.tick()
}

#[cfg(test)]
pub fn merge_if_idle(name: &str, grace: Duration) {
    GAMES.merge_if_idle(name, grace)
}

pub fn save(name: &str) -> Result<String, String> {
    GAMES.save(name)
}

pub fn discard(name: &str) -> Result<(), String> {
    GAMES.discard(name)
}

pub fn info() -> Vec<Info> {
    GAMES.info()
}
