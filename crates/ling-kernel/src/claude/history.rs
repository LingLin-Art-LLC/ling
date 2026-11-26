//! Saved conversations: one file per conversation under home/<user>/.claude-lingos/,
//! sealed with the user's keyring (purpose "claude-history", bound to the file's path)
//! the way Messenger keeps its history -- nothing of it reads back while the screen is
//! locked or another user is signed in. Each holds the title (from the first prompt),
//! the folder it was started in, the model, the token totals and the messages exactly
//! as sent (thinking blocks included, so a resumed conversation continues unchanged).

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use serde_json::{json, Value};

const PURPOSE: &str = "claude-history";

pub fn dir(user: &str) -> String {
    format!("home/{user}/.claude-lingos")
}

fn can_seal(user: &str) -> bool {
    crate::keyring::is_unlocked() && crate::keyring::user().as_deref() == Some(user)
}

/// Save (create or replace) conversation `id`. False when it cannot be sealed now.
pub fn save(user: &str, id: &str, doc: &Value) -> bool {
    if !can_seal(user) {
        return false;
    }
    let d = dir(user);
    if !crate::vfs::is_dir(&d) {
        let _ = crate::vfs::mkdir(&d);
    }
    let path = format!("{d}/{id}.chat");
    let Ok(plain) = serde_json::to_vec(doc) else { return false };
    let Some(blob) = crate::keyring::seal(PURPOSE, path.as_bytes(), &plain) else { return false };
    crate::vfs::write(&path, &blob)
}

pub fn load(user: &str, id: &str) -> Option<Value> {
    if !can_seal(user) {
        return None;
    }
    let path = format!("{}/{id}.chat", dir(user));
    let blob = crate::vfs::read(&path)?;
    let plain = crate::keyring::open(PURPOSE, path.as_bytes(), &blob)?;
    serde_json::from_slice(&plain).ok()
}

pub fn delete(user: &str, id: &str) -> bool {
    crate::vfs::delete(&format!("{}/{id}.chat", dir(user)))
}

/// (id, title, cwd, messages, updated) of every saved conversation, newest first.
pub fn list(user: &str) -> Vec<(String, String, String, usize, u64)> {
    let mut out = Vec::new();
    if !can_seal(user) {
        return out;
    }
    for (name, is_dir, _) in crate::vfs::list(&dir(user)) {
        let Some(id) = name.strip_suffix(".chat") else { continue };
        if is_dir {
            continue;
        }
        if let Some(doc) = load(user, id) {
            let t = doc.get("title").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let c = doc.get("cwd").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let n = doc.get("messages").and_then(|x| x.as_array()).map(|a| a.len()).unwrap_or(0);
            let u = doc.get("updated").and_then(|x| x.as_u64()).unwrap_or(0);
            out.push((String::from(id), t, c, n, u));
        }
    }
    out.sort_by(|a, b| b.4.cmp(&a.4));
    out
}

pub fn new_id() -> String {
    let mut r = [0u8; 4];
    let _ = crate::crypto::random_bytes(&mut r);
    format!("{}-{:02x}{:02x}{:02x}{:02x}", crate::clock::unix_now(), r[0], r[1], r[2], r[3])
}

pub fn doc(title: &str, cwd: &str, model: &str, messages: &[Value], cost: f64, tokens: u64) -> Value {
    json!({
        "title": title, "cwd": cwd, "model": model, "updated": crate::clock::unix_now(),
        "messages": messages, "cost_usd": cost, "tokens": tokens
    })
}
