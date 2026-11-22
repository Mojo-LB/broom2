// users.rs — the accounts and the roles (Settings page, owners only: auth.rs needs() keeps everyone else out).
// Accounts: list, add, change role / reset password, delete. There is always an owner left.
// Roles: owner and staff are built in and fixed; the owner makes others — a name and the permissions it gives
// (auth::Perm) — and gives them to accounts. A role in use cannot be deleted. What a role allows is enforced by the
// server for every request (auth::guard); a change counts from the account's next request.
use axum::{
    Extension, Json, Router,
    extract::State,
    routing::{get, post},
};
use serde::Deserialize;

use crate::SharedState;
use crate::api::{ApiError, bad, ok};
use crate::auth::{
    PERMS, Perm, Perms, Role, STAFF, User, Who, name_ok, new_user, users, with_password,
};

pub fn routes() -> Router<SharedState> {
    Router::new()
        .route("/api/users", get(list).post(set))
        .route("/api/users/delete", post(delete))
        .route("/api/roles", get(roles).post(set_role))
        .route("/api/roles/delete", post(delete_role))
}

/// Most custom roles (a lounge needs a handful).
const ROLES_MAX: usize = 32;

/// GET /api/roles (owner) — {perms: [{id, text}], roles: [{name, perms, builtin, users}]}: owner and staff first.
async fn roles(State(st): State<SharedState>) -> Json<serde_json::Value> {
    let custom = crate::auth::roles(&st);
    let all = users(&st);
    let count = |r: &Role| all.iter().filter(|u| &u.role == r).count();
    let mut list = vec![
        serde_json::json!({"name": "owner", "builtin": true, "users": count(&Role::Owner),
            "perms": PERMS.iter().map(|(p, _)| *p).collect::<Vec<_>>()}),
        serde_json::json!({"name": "staff", "builtin": true, "users": count(&Role::Staff), "perms": STAFF}),
    ];
    for (n, p) in &custom {
        list.push(serde_json::json!({"name": n, "builtin": false, "perms": p,
            "users": count(&Role::Custom(n.clone()))}));
    }
    Json(serde_json::json!({
        "perms": PERMS.iter().map(|(p, t)| serde_json::json!({"id": p, "text": t})).collect::<Vec<_>>(),
        "roles": list,
    }))
}

#[derive(Deserialize)]
struct RoleBody {
    name: String,
    #[serde(default)]
    perms: Vec<Perm>,
}

/// A name a custom role may have: an account-style name that is not a built-in role's.
pub(crate) fn role_name_ok(n: &str) -> bool {
    name_ok(n) && n != "owner" && n != "staff"
}

/// POST /api/roles {name, perms} (owner) — make a role or change what it allows (its accounts: from their next
/// request). No permission at all = an account that can only look.
async fn set_role(
    State(st): State<SharedState>,
    Extension(w): Extension<Who>,
    Json(b): Json<RoleBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let name = b.name.trim().to_lowercase();
    if !role_name_ok(&name) {
        return Err(bad(
            "role name: a-z, 0-9, _ or -, at most 32 (owner and staff are built in and fixed)",
        ));
    }
    let perms: Perms = b.perms.iter().copied().collect();
    let p2 = perms.clone();
    let n2 = name.clone();
    crate::db::edit_json(
        &*st.db,
        "roles",
        move |all: &mut std::collections::BTreeMap<String, Perms>| {
            if !all.contains_key(&n2) && all.len() >= ROLES_MAX {
                return Err(bad(format!("at most {ROLES_MAX} roles")));
            }
            all.insert(n2, p2);
            Ok(())
        },
    )
    .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e))??;
    tracing::warn!("{}: role {name} set to {:?}", w.name, perms);
    Ok(ok())
}

/// POST /api/roles/delete {name} (owner) — not a built-in one, not one an account still has.
async fn delete_role(
    State(st): State<SharedState>,
    Extension(w): Extension<Who>,
    Json(b): Json<NameBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !role_name_ok(&b.name) {
        return Err(bad("owner and staff are built in"));
    }
    let used: Vec<String> = users(&st)
        .into_iter()
        .filter(|u| u.role == Role::Custom(b.name.clone()))
        .map(|u| u.name)
        .collect();
    if !used.is_empty() {
        return Err(bad(format!(
            "role {} is still given to {} — change their role first",
            b.name,
            used.join(", ")
        )));
    }
    let n = b.name.clone();
    crate::db::edit_json(
        &*st.db,
        "roles",
        move |all: &mut std::collections::BTreeMap<String, Perms>| {
            all.remove(&n);
            Ok::<(), ApiError>(())
        },
    )
    .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e))??;
    tracing::warn!("{}: role {} deleted", w.name, b.name);
    Ok(ok())
}

/// GET /api/users — [{name, role}].
async fn list(State(st): State<SharedState>) -> Json<serde_json::Value> {
    Json(serde_json::json!(
        users(&st)
            .iter()
            .map(|u| serde_json::json!({"name": u.name, "role": u.role}))
            .collect::<Vec<_>>()
    ))
}

#[derive(Deserialize)]
struct SetBody {
    name: String,
    role: Role,
    /// The password's verifier, sealed with the caller's session key (pwproof.rs; the password never crosses the
    /// network). New account: required. Existing one: a new password (its sessions end); absent = unchanged.
    #[serde(default)]
    cred: Option<crate::pwproof::Sealed>,
}

/// No owner left after this change.
fn no_owner(v: &[User]) -> bool {
    !v.iter().any(|u| u.role == Role::Owner)
}

/// POST /api/users {name, role, cred?} — add an account, or change one's role / password.
async fn set(
    State(st): State<SharedState>,
    Extension(w): Extension<Who>,
    headers: axum::http::HeaderMap,
    Json(b): Json<SetBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let name = b.name.trim().to_lowercase();
    if !name_ok(&name) {
        return Err(bad("user name: a-z, 0-9, _ or -, at most 32"));
    }
    // A role that exists: a mistyped one would leave the account with no permission at all.
    if let Role::Custom(r) = &b.role
        && !crate::auth::roles(&st).contains_key(r)
    {
        return Err(bad(format!(
            "no role {r} — make it first (Roles, on this page)"
        )));
    }
    let new_pw = b.cred.is_some();
    // Here it would end the caller's own session too; Change password checks the current one and keeps the session.
    if new_pw && name == w.name {
        return Err(bad("your own password: use Change password"));
    }
    let phc = match &b.cred {
        None => None,
        Some(c) => Some(
            crate::auth::session_key(&st, &headers)
                .and_then(|k| crate::pwproof::open(&k, &name, c))
                .ok_or_else(|| bad("password: bad seal — sign in again"))?,
        ),
    };
    // As one step: the "an owner is left" rule holds even when two owners edit at once.
    let added = crate::auth::edit_users(&st, |all| {
        let added = match all.iter().position(|u| u.name == name) {
            Some(i) => {
                let mut u = all[i].clone();
                u.role = b.role.clone();
                if let Some(p) = phc {
                    u = with_password(u, p);
                }
                all[i] = u;
                false
            }
            None => {
                let Some(p) = phc else {
                    return Err(bad("a new account needs a password"));
                };
                all.push(new_user(&name, b.role.clone(), p));
                true
            }
        };
        if no_owner(all) {
            return Err(bad("there must be an owner left"));
        }
        Ok(added)
    })?;
    if !added && new_pw {
        crate::auth::end_sessions(&st, &name); // a new password ends its sessions
    }
    if added {
        tracing::warn!("{}: account {name} added ({:?})", w.name, b.role);
    } else {
        tracing::warn!(
            "{}: account {name} set to {:?}{}",
            w.name,
            b.role,
            if !new_pw { "" } else { ", new password" }
        );
    }
    Ok(ok())
}

#[derive(Deserialize)]
struct NameBody {
    name: String,
}

/// POST /api/users/delete {name} — not your own account, not the last owner. Its sessions end.
async fn delete(
    State(st): State<SharedState>,
    Extension(w): Extension<Who>,
    Json(b): Json<NameBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if b.name == w.name {
        return Err(bad("you can't delete your own account"));
    }
    crate::auth::edit_users(&st, |all| {
        all.retain(|u| u.name != b.name);
        if no_owner(all) {
            return Err(bad("there must be an owner left"));
        }
        Ok(())
    })?;
    crate::auth::end_sessions(&st, &b.name);
    tracing::warn!("{}: account {} deleted", w.name, b.name);
    Ok(ok())
}
