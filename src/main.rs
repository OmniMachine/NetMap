#[macro_use] extern crate rocket;

use rocket::{
    fs::NamedFile,
    http::Status,
    request::{FromRequest, Outcome, Request},
    response::{content::RawJson, status::Created},
    serde::json::Json,
    tokio::{fs, sync::Mutex},
    State,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::HashMap,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

// ── Config ─────────────────────────────────────────────────────────────────

const MAPS_DIR:          &str = "maps";
const HTML_PATH:         &str = "static/netmap.html";
const PRESENCE_TTL_SECS: u64 = 30;

// ── Shared state ───────────────────────────────────────────────────────────

struct AppState {
    maps_dir: PathBuf,
    /// map_id -> { session_id -> last_seen_unix_secs }
    presence: Mutex<HashMap<String, HashMap<String, u64>>>,
}

// ── Wire types ─────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct MapSummary {
    id:         String,
    name:       String,
    updated_at: u64,
}

#[derive(Serialize, Deserialize, Default)]
struct MapMeta {
    name:            String,
    last_session_id: String,
    updated_at:      u64,
}

#[derive(Deserialize)]
struct PresenceIn {
    session_id: String,
    #[serde(default)]
    leaving: bool,
}

#[derive(Serialize)]
struct PresenceOut {
    editors:         usize,
    updated_at:      u64,
    last_session_id: String,
}

// ── Session-ID request guard ───────────────────────────────────────────────
// Extracts the X-Session-Id header; silently defaults to "" if absent.

struct SessionId(String);

#[rocket::async_trait]
impl<'r> FromRequest<'r> for SessionId {
    type Error = ();

    async fn from_request(req: &'r Request<'_>) -> Outcome<Self, ()> {
        let id = req.headers()
            .get_one("X-Session-Id")
            .unwrap_or("")
            .to_string();
        Outcome::Success(SessionId(id))
    }
}

// ── Helpers ────────────────────────────────────────────────────────────────

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn is_valid_id(id: &str) -> bool {
    Uuid::parse_str(id).is_ok()
}

fn map_path(dir: &PathBuf, id: &str) -> PathBuf {
    dir.join(format!("{id}.json"))
}

fn meta_path(dir: &PathBuf, id: &str) -> PathBuf {
    dir.join(format!("{id}.meta.json"))
}

/// Write via temp-file then atomic rename — readers never see a partial write.
async fn write_atomic(path: &PathBuf, data: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, data).await?;
    fs::rename(&tmp, path).await
}

async fn read_meta(dir: &PathBuf, id: &str) -> MapMeta {
    fs::read_to_string(meta_path(dir, id))
        .await
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

async fn write_meta(dir: &PathBuf, id: &str, meta: &MapMeta) {
    if let Ok(s) = serde_json::to_string(meta) {
        let _ = write_atomic(&meta_path(dir, id), &s).await;
    }
}

// ── Handlers ───────────────────────────────────────────────────────────────

/// GET / — serve the single-page app
#[get("/")]
async fn serve_html() -> Option<NamedFile> {
    NamedFile::open(HTML_PATH).await.ok()
}

/// GET /api/maps — list all maps, sorted newest first
#[get("/api/maps")]
async fn list_maps(state: &State<AppState>) -> Json<Vec<MapSummary>> {
    let mut summaries = Vec::new();

    let mut dir = match fs::read_dir(&state.maps_dir).await {
        Ok(d)  => d,
        Err(_) => return Json(summaries),
    };

    while let Ok(Some(entry)) = dir.next_entry().await {
        let fname = entry.file_name().to_string_lossy().into_owned();

        // Read only the tiny meta files — avoids loading full map JSON for the list
        if !fname.ends_with(".meta.json") {
            continue;
        }

        let id = fname.trim_end_matches(".meta.json").to_string();
        if !is_valid_id(&id) {
            continue;
        }

        // Skip if the corresponding map file is missing
        if !map_path(&state.maps_dir, &id).exists() {
            continue;
        }

        let meta = read_meta(&state.maps_dir, &id).await;
        summaries.push(MapSummary {
            id,
            name: if meta.name.is_empty() { "Untitled Topology".into() } else { meta.name },
            updated_at: meta.updated_at,
        });
    }

    summaries.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    Json(summaries)
}

/// GET /api/maps/<id> — return raw map JSON
#[get("/api/maps/<id>")]
async fn get_map(id: &str, state: &State<AppState>) -> Result<RawJson<String>, Status> {
    if !is_valid_id(id) {
        return Err(Status::BadRequest);
    }

    fs::read_to_string(map_path(&state.maps_dir, id))
        .await
        .map(RawJson)
        .map_err(|_| Status::NotFound)
}

/// POST /api/maps — create a new empty map, return its id
#[post("/api/maps")]
async fn create_map(state: &State<AppState>) -> Result<Created<Json<serde_json::Value>>, Status> {
    let id  = Uuid::new_v4().to_string();
    let now = now_secs();

    let initial = json!({
        "version": 2,
        "name":    "Untitled Topology",
        "nodes":   [],
        "edges":   []
    });

    write_atomic(&map_path(&state.maps_dir, &id), &initial.to_string())
        .await
        .map_err(|_| Status::InternalServerError)?;

    write_meta(&state.maps_dir, &id, &MapMeta {
        name:            "Untitled Topology".into(),
        last_session_id: String::new(),
        updated_at:      now,
    })
    .await;

    Ok(Created::new(format!("/api/maps/{id}")).body(Json(json!({ "id": id }))))
}

/// PUT /api/maps/<id> — overwrite map with the JSON body
#[put("/api/maps/<id>", format = "json", data = "<body>")]
async fn update_map(
    id:      &str,
    body:    Json<serde_json::Value>,
    session: SessionId,
    state:   &State<AppState>,
) -> Status {
    if !is_valid_id(id) {
        return Status::BadRequest;
    }

    if !map_path(&state.maps_dir, id).exists() {
        return Status::NotFound;
    }

    let value = body.into_inner();

    // Extract name before we consume `value` for serialisation
    let name = value["name"].as_str().map(String::from);

    let json_str = match serde_json::to_string(&value) {
        Ok(s)  => s,
        Err(_) => return Status::InternalServerError,
    };

    if write_atomic(&map_path(&state.maps_dir, id), &json_str)
        .await
        .is_err()
    {
        return Status::InternalServerError;
    }

    let final_name = match name {
        Some(n) => n,
        None    => read_meta(&state.maps_dir, id).await.name,
    };

    write_meta(&state.maps_dir, id, &MapMeta {
        name:            final_name,
        last_session_id: session.0,
        updated_at:      now_secs(),
    })
    .await;

    Status::NoContent
}

/// DELETE /api/maps/<id> — remove map and its meta
#[delete("/api/maps/<id>")]
async fn delete_map(id: &str, state: &State<AppState>) -> Status {
    if !is_valid_id(id) {
        return Status::BadRequest;
    }

    let _ = fs::remove_file(meta_path(&state.maps_dir, id)).await;

    match fs::remove_file(map_path(&state.maps_dir, id)).await {
        Ok(_)  => Status::NoContent,
        Err(_) => Status::NotFound,
    }
}

/// POST /api/maps/<id>/presence — heartbeat or leave signal
#[post("/api/maps/<id>/presence", format = "json", data = "<body>")]
async fn post_presence(
    id:    &str,
    body:  Json<PresenceIn>,
    state: &State<AppState>,
) -> Status {
    if !is_valid_id(id) {
        return Status::BadRequest;
    }

    let payload   = body.into_inner();
    let mut store = state.presence.lock().await;

    if payload.leaving {
        if let Some(sessions) = store.get_mut(id) {
            sessions.remove(&payload.session_id);
        }
    } else {
        store
            .entry(id.to_string())
            .or_default()
            .insert(payload.session_id, now_secs());
    }

    Status::NoContent
}

/// GET /api/maps/<id>/presence — active editor count + last save metadata
#[get("/api/maps/<id>/presence")]
async fn get_presence(
    id:      &str,
    session: SessionId,
    state:   &State<AppState>,
) -> Result<Json<PresenceOut>, Status> {
    if !is_valid_id(id) {
        return Err(Status::BadRequest);
    }

    let now = now_secs();

    let editors = {
        let mut store = state.presence.lock().await;
        let sessions  = store.entry(id.to_string()).or_default();
        sessions.retain(|_, last| now - *last < PRESENCE_TTL_SECS);
        sessions
            .iter()
            .filter(|(sid, _)| sid.as_str() != session.0)
            .count()
    };

    let meta = read_meta(&state.maps_dir, id).await;

    Ok(Json(PresenceOut {
        editors,
        updated_at:      meta.updated_at,
        last_session_id: meta.last_session_id,
    }))
}

// ── Launch ─────────────────────────────────────────────────────────────────

#[rocket::main]
async fn main() -> Result<(), rocket::Error> {
    let maps_dir = PathBuf::from(MAPS_DIR);
    fs::create_dir_all(&maps_dir)
        .await
        .expect("could not create maps/ directory");

    let _rocket = rocket::build()
        .manage(AppState {
            maps_dir,
            presence: Mutex::new(HashMap::new()),
        })
        .mount("/", routes![
            serve_html,
            list_maps,
            get_map,
            create_map,
            update_map,
            delete_map,
            post_presence,
            get_presence,
        ])
        .launch()
        .await?;

    Ok(())
}
