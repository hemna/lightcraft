//! Talking to somebody's Immich from the engine: configuring servers, browsing them, and
//! importing photos from a self-hosted [Immich](https://immich.app) server.
//!
//! The servers are the whole configuration — URL, display name, and the API key the user created
//! in Immich. Nothing here knows any particular instance: no default host, no fallback, no
//! admin assumptions. Keys live in the user's per-user settings file
//! (`config_dir()/immich.json`, created `0600` on Unix), **never in the library**: the library
//! folder is what people back up and sync, and a key must not travel with it. The file holds the
//! keys unencrypted in v1; the OS-keychain follow-up is tracked with the design spec.
//!
//! Network calls happen on whatever thread runs the command — native only, like
//! `lightcraft-fetch`. On wasm this module ships the settings commands only; the UI's Immich
//! dialog runs its searches and downloads on its own worker, so these commands are for the CLI
//! and MCP, where waiting for the answer is the point.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{CommandSpec, always, bad, cmd, str_param};
use crate::{EngineError, Result, Session};

/// One Immich server the user configured: where it is, what to call it, and the key they created
/// in Immich's Settings ▸ API keys. `verified` is set when a test reached the server and it
/// answered — the UI keeps File ▸ Import from Immich disabled until some server has it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ImmichServer {
    pub name: String,
    pub url: String,
    pub api_key: String,
    pub verified: bool,
}

/// The Immich part of the user's per-user settings (the config folder), not of a library.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ImmichPrefs {
    pub servers: Vec<ImmichServer>,
}

const MAX_SERVERS: usize = 32;

/// Where the user's Immich settings (the servers and their API keys) are kept: the per-user
/// config folder, never inside a library. A session may override the path (`immich_store`) for
/// tests, so they never read or write the real file.
fn store_path(s: &Session) -> Option<std::path::PathBuf> {
    s.immich_store.clone().or_else(|| crate::config::config_dir().map(|d| d.join("immich.json")))
}

/// The configured servers. A missing file is an empty list; an unreadable or corrupt one is
/// reported and starts empty — the next save replaces it, and no command panics on it.
pub fn load(s: &Session) -> Vec<ImmichServer> {
    let Some(p) = store_path(s) else {
        return Vec::new();
    };
    match std::fs::read_to_string(&p) {
        Ok(text) => match serde_json::from_str::<ImmichPrefs>(&text) {
            Ok(v) => v.servers,
            Err(e) => {
                log::warn!("immich: the settings file {} could not be read ({e}); starting empty", p.display());
                Vec::new()
            }
        },
        Err(_) => Vec::new(),
    }
}

/// Persist the configured servers. On Unix the file is created `0600` at birth — the temp file
/// and the renamed result alike — so a key never exists in a mode the owner did not choose.
pub fn save(s: &Session) -> Result<()> {
    let p = store_path(s).ok_or_else(|| EngineError::Other("immich: no per-user settings folder is available on this platform".to_string()))?;
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).map_err(|e| EngineError::Other(format!("immich: could not create the settings folder: {e}")))?;
    }
    let v = serde_json::to_vec_pretty(&ImmichPrefs { servers: s.immich_servers.clone() })
        .map_err(|e| EngineError::Other(format!("immich: could not encode the settings: {e}")))?;
    write_private(&p, &v)
}

/// Write `data` to `path` atomically (temp file in the same folder, then rename), with the file
/// born owner-only on Unix. The temp name is unique per call, so a crashed writer's leftover
/// cannot make the next save fail.
fn write_private(path: &std::path::Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static STORE_SEQ: AtomicU64 = AtomicU64::new(0);
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "immich.json".to_string());
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let tmp = path.with_file_name(format!(".{name}.{}-{nanos:x}-{:x}.tmp", std::process::id(), STORE_SEQ.fetch_add(1, Ordering::Relaxed)));
    let mut f = {
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)
        }
        #[cfg(not(unix))]
        {
            std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp)
        }
    }
    .map_err(|e| EngineError::Other(format!("immich: could not create the settings file: {e}")))?;
    if let Err(e) = f.write_all(data) {
        let _ = std::fs::remove_file(&tmp);
        return Err(EngineError::Other(format!("immich: could not write the settings file: {e}")));
    }
    if let Err(e) = f.sync_all() {
        let _ = std::fs::remove_file(&tmp);
        return Err(EngineError::Other(format!("immich: could not sync the settings file: {e}")));
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(EngineError::Other(format!("immich: could not save the settings file: {e}")));
    }
    Ok(())
}

fn valid_url(url: &str) -> bool {
    url.len() <= 2000
        && (url.starts_with("http://") || url.starts_with("https://"))
        && url.chars().all(|c| !c.is_control() && !c.is_whitespace())
        && url.find("//").is_some_and(|i| !url[i + 2..].starts_with('/'))
}

/// `http://host:2283/prefix` → `host:2283` — the display name when the user did not pick one.
fn host_of(url: &str) -> String {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let host = rest.split('/').next().unwrap_or_default();
    let host = host.rsplit('@').next().unwrap_or(host);
    if host.is_empty() { url.to_string() } else { host.to_string() }
}

impl ImmichServer {
    fn normalise(url: &str) -> String {
        url.trim().trim_end_matches('/').to_string()
    }
    fn matches(&self, sel: &str) -> bool {
        !sel.is_empty() && (self.name.eq_ignore_ascii_case(sel) || Self::normalise(&self.url) == Self::normalise(sel))
    }
    /// What commands may show: everything except the key.
    fn public(&self) -> Value {
        json!({
            "name": self.name,
            "url": self.url,
            "insecure": !self.url.starts_with("https://"),
            "key": if self.api_key.is_empty() { "missing" } else { "set" },
            "verified": self.verified,
        })
    }
}

fn current(s: &Session) -> Value {
    json!({ "servers": s.immich_servers.iter().map(ImmichServer::public).collect::<Vec<_>>() })
}

/// Read, add and remove servers. The key goes in and never comes back out.
fn servers(s: &mut Session, p: &Value) -> Result<Value> {
    const CID: &str = "immich.servers";
    if let Some(a) = p.get("add") {
        let url = str_param(a, "url").unwrap_or_default().trim().to_string();
        if !valid_url(&url) {
            return Err(bad(CID, "`url` must be the http:// or https:// address of your Immich server (e.g. https://photos.lan:2283)"));
        }
        let key = str_param(a, "apiKey").unwrap_or_default().trim().to_string();
        if key.is_empty() || key.len() > 200 || key.chars().any(char::is_control) {
            return Err(bad(CID, "`apiKey` is required — create one in Immich under Settings ▸ API keys"));
        }
        let name = match str_param(a, "name").map(str::trim).filter(|n| !n.is_empty()) {
            Some(n) if n.chars().count() <= 100 && !n.chars().any(char::is_control) => n.to_string(),
            Some(_) => return Err(bad(CID, "`name` is too long")),
            None => host_of(&url),
        };
        // a fresh entry is unverified — File ▸ Import from Immich unlocks only after a Test
        let entry = ImmichServer { name, url, api_key: key, verified: false };
        match s.immich_servers.iter_mut().find(|x| ImmichServer::normalise(&x.url) == ImmichServer::normalise(&entry.url)) {
            Some(existing) => *existing = entry,
            None => {
                if s.immich_servers.len() >= MAX_SERVERS {
                    return Err(bad(CID, "too many servers configured (32)"));
                }
                if s.immich_servers.iter().any(|x| x.name.eq_ignore_ascii_case(&entry.name)) {
                    return Err(bad(CID, format!("a server named `{}` already exists — remove it first", entry.name)));
                }
                s.immich_servers.push(entry);
            }
        }
        s.save_immich()?;
    }
    if let Some(rm) = p.get("remove") {
        let sel = [str_param(rm, "name"), str_param(rm, "url")].into_iter().flatten().next().unwrap_or_default().to_string();
        if sel.trim().is_empty() {
            return Err(bad(CID, "`remove` needs a `name` or `url`"));
        }
        let before = s.immich_servers.len();
        s.immich_servers.retain(|x| !x.matches(sel.trim()));
        if s.immich_servers.len() == before {
            return Err(bad(CID, format!("no Immich server `{sel}`")));
        }
        s.save_immich()?;
    }
    Ok(current(s))
}

#[cfg(not(target_arch = "wasm32"))]
fn net(cid: &'static str) -> impl Fn(lightcraft_immich::Error) -> EngineError {
    move |e| EngineError::Other(format!("{cid}: {e}"))
}

#[cfg(not(target_arch = "wasm32"))]
fn server_of<'a>(s: &'a Session, p: &Value, cid: &'static str) -> Result<&'a ImmichServer> {
    let sel = str_param(p, "server").unwrap_or("").trim().to_string();
    if s.immich_servers.is_empty() {
        return Err(bad(cid, "no Immich server is configured — immich.servers {add: {url, apiKey}}"));
    }
    if sel.is_empty() {
        return s.immich_servers.first().ok_or_else(|| bad(cid, "no server"));
    }
    s.immich_servers.iter().find(|x| x.matches(&sel)).ok_or_else(|| {
        let names: Vec<&str> = s.immich_servers.iter().map(|x| x.name.as_str()).collect();
        bad(cid, format!("unknown Immich server `{sel}` (configured: {})", names.join(", ")))
    })
}

/// An ISO-8601-ish date parameter: present, bounded, and free of control characters.
#[cfg(not(target_arch = "wasm32"))]
fn date_param(p: &Value, key: &str, cid: &'static str) -> Result<Option<String>> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(d)) => {
            let d = d.trim();
            if d.is_empty() {
                return Ok(None);
            }
            if d.len() > 64 || d.chars().any(char::is_control) {
                return Err(bad(cid, format!("`{key}` must be an ISO 8601 date like 2026-01-01 or 2026-01-01T00:00:00Z")));
            }
            Ok(Some(d.to_string()))
        }
        _ => Err(bad(cid, format!("`{key}` must be a date string"))),
    }
}

/// Is the Immich server up, and what does it report? The settings dialog's "Test" button.
/// A passing test marks the server verified (File ▸ Import from Immich needs one that is).
#[cfg(not(target_arch = "wasm32"))]
fn test(s: &mut Session, p: &Value) -> Result<Value> {
    const CID: &str = "immich.test";
    let (url, key) = {
        let srv = server_of(s, p, CID)?;
        (srv.url.clone(), srv.api_key.clone())
    };
    let c = lightcraft_immich::Client::new(&url, &key, Default::default()).map_err(net(CID))?;
    let pong = c.ping().map_err(net(CID))?;
    let version = c.version().map_err(net(CID))?;
    mark_verified(s, &url);
    s.save_immich()?;
    Ok(json!({ "ok": true, "pong": pong, "version": version.to_string() }))
}

/// Record that `server` passed a test. The UI runs its tests on a worker thread (the UI thread
/// never waits for the network), so it calls this with the answer; `immich.test` calls it itself.
/// No network happens here — the claim is only as good as the test that made it.
fn verify(s: &mut Session, p: &Value) -> Result<Value> {
    const CID: &str = "immich.verify";
    let sel = str_param(p, "server").unwrap_or("").trim().to_string();
    if sel.is_empty() {
        return Err(bad(CID, "`server` is the name or URL of a configured Immich server"));
    }
    let url = s.immich_servers.iter().find(|x| x.matches(&sel)).map(|x| x.url.clone()).ok_or_else(|| {
        let names: Vec<&str> = s.immich_servers.iter().map(|x| x.name.as_str()).collect();
        bad(CID, format!("unknown Immich server `{sel}` (configured: {})", names.join(", ")))
    })?;
    mark_verified(s, &url);
    s.save_immich()?;
    Ok(current(s))
}

/// Flag every entry with this URL as verified (there is at most one — `add` replaces by URL).
fn mark_verified(s: &mut Session, url: &str) {
    for x in &mut s.immich_servers {
        if ImmichServer::normalise(&x.url) == ImmichServer::normalise(url) {
            x.verified = true;
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn albums(s: &mut Session, p: &Value) -> Result<Value> {
    const CID: &str = "immich.albums";
    let srv = server_of(s, p, CID)?;
    let c = lightcraft_immich::Client::new(&srv.url, &srv.api_key, Default::default()).map_err(net(CID))?;
    let list = c.albums().map_err(net(CID))?;
    Ok(json!({
        "albums": list.iter().map(|a| json!({ "id": a.id, "name": a.album_name, "assetCount": a.asset_count })).collect::<Vec<_>>(),
    }))
}

/// One page of a search. Answers are the crate's tolerant DTOs, so a 3.x server that adds or
/// drops fields changes what is `None`, never the shape of this answer.
#[cfg(not(target_arch = "wasm32"))]
fn browse(s: &mut Session, p: &Value) -> Result<Value> {
    use lightcraft_immich::SearchQuery;
    const CID: &str = "immich.browse";
    let srv = server_of(s, p, CID)?;
    let c = lightcraft_immich::Client::new(&srv.url, &srv.api_key, Default::default()).map_err(net(CID))?;
    let rating = match p.get("rating") {
        None | Some(Value::Null) => None,
        Some(v) => match v.as_u64() {
            Some(n @ 1..=5) => Some(n as u32),
            _ => return Err(bad(CID, "`rating` is a whole number 1…5")),
        },
    };
    let mut q = SearchQuery {
        is_favorite: match p.get("isFavorite") {
            None | Some(Value::Null) => None,
            Some(v) => Some(v.as_bool().ok_or_else(|| bad(CID, "`isFavorite` must be true or false"))?),
        },
        rating,
        created_after: date_param(p, "createdAfter", CID)?,
        created_before: date_param(p, "createdBefore", CID)?,
        updated_after: date_param(p, "updatedAfter", CID)?,
        asset_type: match p.get("type") {
            None | Some(Value::Null) => None,
            Some(Value::String(t)) if t == "IMAGE" || t == "VIDEO" => Some(t.clone()),
            Some(_) => return Err(bad(CID, "`type` is \"IMAGE\" or \"VIDEO\"")),
        },
        page: p.get("page").and_then(Value::as_u64).unwrap_or(1).clamp(1, 100_000) as u32,
        page_size: p.get("pageSize").and_then(Value::as_u64).unwrap_or(60).clamp(1, 1000) as u32,
        ..Default::default()
    };
    if let Some(album) = str_param(p, "album").map(str::trim).filter(|a| !a.is_empty()) {
        let albums = c.albums().map_err(net(CID))?;
        match albums.iter().find(|a| a.id == album || a.album_name.eq_ignore_ascii_case(album)) {
            Some(a) => q.album_ids = vec![a.id.clone()],
            None => return Err(bad(CID, format!("no album `{album}` on the server — immich.albums lists them"))),
        }
    }
    let page = c.search(&q).map_err(net(CID))?;
    Ok(json!({
        "page": page.page,
        "maybeMore": page.maybe_more,
        "assets": page.items.iter().map(|a| json!({
            "id": a.id,
            "checksum": a.checksum,
            "fileName": a.original_file_name,
            "createdAt": a.file_created_at,
            "isFavorite": a.is_favorite,
            "rating": a.rating,
        })).collect::<Vec<_>>(),
    }))
}

/// Strip a server-supplied name down to one safe file name for the staging folder.
#[cfg(not(target_arch = "wasm32"))]
fn safe_file_name(given: &str, id: &str) -> String {
    let base = given.rsplit(['/', '\\']).next().unwrap_or(given);
    let mut out: String =
        base.chars().map(|c| if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') { '_' } else { c }).collect();
    let trimmed = out.trim_matches(|c| c == ' ' || c == '.');
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        // a byte cut here would panic on a multi-byte id (the name is agent- or server-supplied)
        let short: String = id.chars().take(64).collect();
        return format!("immich-{short}");
    }
    if out.chars().count() > 200 {
        out = out.chars().take(200).collect();
    }
    out
}

/// One fresh staging folder per import: a per-process sequence plus the clock make the name
/// unpredictable (a pid alone is guessable), and on Unix the folder is created `0700`, so a
/// neighbour on the same box cannot list it or plant a file in it before the import pipeline
/// reads it. It is removed again when the import ends.
/// Not inside the library — the pipeline never copies a file that already sits inside the
/// library folder (import.rs treats it as added-in-place), and `copy` is the point here.
#[cfg(not(target_arch = "wasm32"))]
fn staging_dir() -> Result<std::path::PathBuf> {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static STAGING_SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let dir =
        std::env::temp_dir().join(format!("lightcraft-immich-{}-{nanos:x}-{:x}", std::process::id(), STAGING_SEQ.fetch_add(1, Ordering::Relaxed)));
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(&dir).map_err(|e| EngineError::Other(format!("immich.import: could not create the staging folder: {e}")))?;
    Ok(dir)
}

#[cfg(not(target_arch = "wasm32"))]
fn staged_path(staging: &std::path::Path, name: &str) -> std::path::PathBuf {
    let stem = name.rsplit_once('.').map(|(s, e)| (s.to_string(), format!(".{e}"))).unwrap_or_else(|| (name.to_string(), String::new()));
    for n in 0..1000u32 {
        let candidate = if n == 0 { staging.join(name) } else { staging.join(format!("{}-{n}{}", stem.0, stem.1)) };
        if !candidate.exists() {
            return candidate;
        }
    }
    staging.join(format!("immich-{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)))
}

/// Download the named assets, then run them through the untouched import pipeline (probe,
/// content-hash dedupe, undoable add/copy). `copy` (the default) places the originals in the
/// library like any other import; `add` keeps the staged files in place.
#[cfg(not(target_arch = "wasm32"))]
fn import(s: &mut Session, p: &Value) -> Result<Value> {
    const CID: &str = "immich.import";
    let ids: Vec<String> = match p.get("ids").and_then(Value::as_array) {
        Some(a) if !a.is_empty() => {
            let mut v = Vec::with_capacity(a.len());
            for x in a {
                let Some(x) = x.as_str().filter(|x| !x.is_empty() && x.len() <= 500 && !x.chars().any(char::is_control)) else {
                    return Err(bad(CID, "`ids` must be asset id strings from immich.browse"));
                };
                v.push(x.to_string());
            }
            v
        }
        _ => return Err(bad(CID, "`ids` — asset ids from immich.browse — is required")),
    };
    if ids.len() > 10_000 {
        return Err(bad(CID, "too many ids at once (10 000)"));
    }
    let mode = str_param(p, "mode").unwrap_or("copy");
    if mode != "copy" && mode != "add" {
        return Err(bad(CID, "`mode` is \"copy\" (default — into the library) or \"add\" (keep the downloaded files where they are)"));
    }
    let srv = server_of(s, p, CID)?;
    let c = lightcraft_immich::Client::new(&srv.url, &srv.api_key, Default::default()).map_err(net(CID))?;
    let staging = staging_dir()?;
    let mut paths: Vec<String> = Vec::new();
    let mut failed: Vec<Value> = Vec::new();
    for id in &ids {
        let given = c.asset(id).map(|a| a.original_file_name).unwrap_or_default();
        let dest = staged_path(&staging, &safe_file_name(&given, id));
        match std::fs::File::create(&dest) {
            Ok(mut f) => match c.download_original(id, &mut f, |_, _| {}) {
                Ok(n) if n > 0 => paths.push(dest.to_string_lossy().into_owned()),
                Ok(_) => {
                    let _ = std::fs::remove_file(&dest);
                    failed.push(json!({ "id": id, "error": "the server sent an empty file" }));
                }
                Err(e) => {
                    let _ = std::fs::remove_file(&dest);
                    failed.push(json!({ "id": id, "error": e.to_string() }));
                }
            },
            Err(e) => failed.push(json!({ "id": id, "error": format!("could not write the download: {e}") })),
        }
    }
    if paths.is_empty() {
        let _ = std::fs::remove_dir(&staging);
        return Err(EngineError::Other(format!("{CID}: nothing could be downloaded — {failed:?}")));
    }
    let mut params = json!({ "paths": paths, "mode": mode });
    for key in ["album", "albumName"] {
        if let Some(v) = p.get(key) {
            params[key] = v.clone();
        }
    }
    let report = s.execute("library.import", &params)?;
    if mode == "copy" {
        // the bytes are in the library now; the staged copies have no further use
        for path in &paths {
            if let Err(e) = std::fs::remove_file(path) {
                log::warn!("immich: staging cleanup: {path}: {e}");
            }
        }
        let _ = std::fs::remove_dir(&staging);
    }
    Ok(json!({ "report": report, "failed": failed }))
}

pub fn specs() -> Vec<CommandSpec> {
    let mut v = vec![
        cmd!(
            query "immich.servers",
            "Immich Servers",
            [],
            None,
            "{add?: {url, apiKey, name?}, remove?: {name|url}} — the self-hosted Immich servers this user talks to; keys live in the per-user settings file (config folder, 0600) — never in a library — unencrypted, and are never echoed. A new or re-added server starts unverified → {servers: [{name, url, insecure, key, verified}]}",
            always,
            servers
        ),
        cmd!(
            query "immich.verify",
            "Verify Immich Server",
            [],
            None,
            "{server} — record that the server passed a test (the UI tests on a worker thread and calls this with the answer; immich.test marks it itself). No network: the claim is only as good as the test behind it → {servers}",
            always,
            verify
        ),
    ];
    #[cfg(not(target_arch = "wasm32"))]
    {
        v.push(cmd!(
            query "immich.test",
            "Test Immich Server",
            [],
            None,
            "{server?} — is it Immich, and what version → {ok, pong, version}",
            always,
            test
        ));
        v.push(cmd!(
            query "immich.albums",
            "Immich Albums",
            [],
            None,
            "{server?} — the server's albums → {albums: [{id, name, assetCount}]}",
            always,
            albums
        ));
        v.push(cmd!(
            query "immich.browse",
            "Browse Immich",
            [],
            None,
            "{server?, album? (name or id), isFavorite?, rating? (1-5), createdAfter?/createdBefore? (ISO date), updatedAfter?, type? (IMAGE|VIDEO), page? (1-based), pageSize? (1-1000, default 60)} — one page of assets on the server → {page, maybeMore, assets: [{id, checksum, fileName, createdAt, isFavorite, rating}]}",
            always,
            browse
        ));
        v.push(cmd!(
            "immich.import",
            "Import from Immich",
            [],
            None,
            "{server?, ids: [assetId...], mode? (\"copy\" default, into the library; \"add\" keeps the files in the staging folder), album?|albumName?} — download the assets and run the ordinary import over them (dedupe, undo) → {report, failed: [{id, error}]}",
            always,
            import
        ));
    }
    v
}
