// notices.rs — what the admin should know without reading the log: a short list shown at the top of every page of
// the web admin until it is dismissed.
// - A notice is one line with a time and a key. The same key again (the same machine refused for the same reason)
//   updates that line and counts, instead of adding lines: a PC asking every few seconds is one notice.
// - Kept in the config table (`notices`), at most KEEP of them, so a restart of the server does not lose them — the
//   one about an upgrade has to be seen after it.
// - Read by every signed-in account; dismissed by the owner (auth::needs: a change is the owner's).
// What ends up here: a machine refused its boot / its key / its "setup done" (another address, a server restart
// during its first boot), a machine that ran out of room and restarted, the web admin moving to HTTPS on an upgrade.
// Everything here is in the log too; this is only where an admin looks first.
use axum::{
    Json, Router,
    extract::State,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};

use crate::SharedState;
use crate::api::{ApiError, ise};

const KEY: &str = "notices";
/// The newest ones kept.
const KEEP: usize = 50;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub(crate) struct Notice {
    pub id: u64,
    /// Unix time it was last seen.
    pub at: u64,
    /// What it is about, for merging repeats (not shown).
    pub key: String,
    pub text: String,
    /// Times seen.
    #[serde(default)]
    pub n: u32,
}

#[derive(Serialize, Deserialize, Default)]
struct Store {
    next: u64,
    list: Vec<Notice>,
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `text` under `key` at time `at`, into `list`: a repeat of a key moves to the end with the new text and time.
pub(crate) fn put(list: &mut Vec<Notice>, next: &mut u64, key: &str, text: &str, at: u64) {
    let n = match list.iter().position(|x| x.key == key) {
        Some(i) => list.remove(i).n,
        None => 0,
    };
    *next += 1;
    list.push(Notice {
        id: *next,
        at,
        key: key.to_string(),
        text: text.chars().take(400).collect(),
        n: n + 1,
    });
    if list.len() > KEEP {
        let over = list.len() - KEEP;
        list.drain(..over);
    }
}

/// Tell the admin (and the log, as a warning). Never fails the caller.
pub(crate) fn warn(st: &SharedState, key: &str, text: &str) {
    tracing::warn!("{text}");
    add(st, key, text);
}

/// Tell the admin without a log line (the caller logs its own).
pub(crate) fn add(st: &SharedState, key: &str, text: &str) {
    let _ = crate::db::edit_json(&*st.db, KEY, |s: &mut Store| {
        put(&mut s.list, &mut s.next, key, text, now());
        Ok::<(), String>(())
    });
}

fn list(st: &SharedState) -> Vec<Notice> {
    serde_json::from_str::<Store>(&st.db.get_config(KEY, "{}"))
        .map(|s| s.list)
        .unwrap_or_default()
}

pub fn routes() -> Router<SharedState> {
    Router::new()
        .route("/api/notices", get(all))
        .route("/api/notices/dismiss", post(dismiss))
}

/// GET /api/notices — [{id, at, text, n}], oldest first.
async fn all(State(st): State<SharedState>) -> Json<Vec<Notice>> {
    Json(list(&st))
}

#[derive(Deserialize)]
struct DismissBody {
    /// One notice; without it, all of them.
    #[serde(default)]
    id: Option<u64>,
}

/// POST /api/notices/dismiss {id?} (owner).
async fn dismiss(
    State(st): State<SharedState>,
    Json(b): Json<DismissBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    crate::db::edit_json(&*st.db, KEY, |s: &mut Store| {
        match b.id {
            Some(id) => s.list.retain(|x| x.id != id),
            None => s.list.clear(),
        }
        Ok::<(), String>(())
    })
    .map_err(ise)?
    .map_err(ise)?;
    Ok(crate::api::ok())
}

#[cfg(test)]
#[path = "../tests/unit/notices.rs"]
mod tests;
