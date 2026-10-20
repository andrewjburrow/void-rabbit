use crate::{auth::actor, error::AppError, AppState};
use actix_web::{web, HttpRequest, HttpResponse};
use serde::{Deserialize, Serialize};
#[derive(Serialize, Deserialize, sqlx::FromRow)]
pub struct Preferences {
    pub theme: String,
    pub accent: String,
    pub sound_enabled: bool,
    pub sound_name: String,
}
pub async fn initialize(pool: &sqlx::SqlitePool) -> Result<(), AppError> {
    sqlx::query("CREATE TABLE IF NOT EXISTS user_preferences(user_id INTEGER PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,theme TEXT NOT NULL DEFAULT 'dark',accent TEXT NOT NULL DEFAULT '#478061',sound_enabled INTEGER NOT NULL DEFAULT 0,sound_name TEXT NOT NULL DEFAULT '',sound_mime TEXT NOT NULL DEFAULT '',sound_data BLOB)").execute(pool).await?;
    sqlx::query("CREATE TABLE IF NOT EXISTS user_map_views(user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,map_id INTEGER NOT NULL REFERENCES topology_maps(id) ON DELETE CASCADE,view_json TEXT NOT NULL,updated_at INTEGER NOT NULL DEFAULT(unixepoch()),PRIMARY KEY(user_id,map_id))").execute(pool).await?;
    Ok(())
}
async fn ensure(pool: &sqlx::SqlitePool, id: i64) -> Result<(), AppError> {
    sqlx::query("INSERT OR IGNORE INTO user_preferences(user_id) VALUES (?)")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}
async fn get(req: HttpRequest, state: web::Data<AppState>) -> Result<HttpResponse, AppError> {
    let id = actor(&req)?.id;
    ensure(&state.pool, id).await?;
    let prefs: Preferences = sqlx::query_as(
        "SELECT theme,accent,sound_enabled,sound_name FROM user_preferences WHERE user_id=?",
    )
    .bind(id)
    .fetch_one(&state.pool)
    .await?;
    Ok(HttpResponse::Ok().json(prefs))
}
async fn save(
    req: HttpRequest,
    state: web::Data<AppState>,
    input: web::Json<Preferences>,
) -> Result<HttpResponse, AppError> {
    let id = actor(&req)?.id;
    if !["dark", "light"].contains(&input.theme.as_str())
        || input.accent.len() != 7
        || !input.accent.starts_with('#')
        || !input.accent[1..].bytes().all(|c| c.is_ascii_hexdigit())
    {
        return Err(AppError::Validation("Invalid theme or color".into()));
    }
    ensure(&state.pool, id).await?;
    sqlx::query("UPDATE user_preferences SET theme=?,accent=?,sound_enabled=? WHERE user_id=?")
        .bind(&input.theme)
        .bind(&input.accent)
        .bind(input.sound_enabled)
        .bind(id)
        .execute(&state.pool)
        .await?;
    Ok(HttpResponse::NoContent().finish())
}
async fn upload(
    req: HttpRequest,
    state: web::Data<AppState>,
    body: web::Bytes,
) -> Result<HttpResponse, AppError> {
    let id = actor(&req)?.id;
    if body.len() > 5 * 1024 * 1024 || body.len() < 12 {
        return Err(AppError::Validation(
            "Upload an audio file up to 5 MB".into(),
        ));
    }
    let mime = if body.starts_with(b"RIFF") && body.get(8..12) == Some(b"WAVE") {
        "audio/wav"
    } else if body.starts_with(b"OggS") {
        "audio/ogg"
    } else if body.starts_with(b"ID3") || (body[0] == 255 && body[1] & 224 == 224) {
        "audio/mpeg"
    } else {
        return Err(AppError::Validation(
            "Choose a WAV, MP3, or OGG sound".into(),
        ));
    };
    let name = req
        .headers()
        .get("X-Filename")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("Custom sound")
        .chars()
        .filter(|c| !c.is_control())
        .take(100)
        .collect::<String>();
    ensure(&state.pool, id).await?;
    sqlx::query(
        "UPDATE user_preferences SET sound_data=?,sound_mime=?,sound_name=? WHERE user_id=?",
    )
    .bind(body.as_ref())
    .bind(mime)
    .bind(name)
    .bind(id)
    .execute(&state.pool)
    .await?;
    Ok(HttpResponse::NoContent().finish())
}
async fn sound(req: HttpRequest, state: web::Data<AppState>) -> Result<HttpResponse, AppError> {
    let id = actor(&req)?.id;
    let row: Option<(String, Option<Vec<u8>>)> =
        sqlx::query_as("SELECT sound_mime,sound_data FROM user_preferences WHERE user_id=?")
            .bind(id)
            .fetch_optional(&state.pool)
            .await?;
    if let Some((mime, Some(bytes))) = row {
        Ok(HttpResponse::Ok().content_type(mime).body(bytes))
    } else {
        Err(AppError::NotFound)
    }
}
async fn clear(req: HttpRequest, state: web::Data<AppState>) -> Result<HttpResponse, AppError> {
    sqlx::query(
        "UPDATE user_preferences SET sound_data=NULL,sound_mime='',sound_name='' WHERE user_id=?",
    )
    .bind(actor(&req)?.id)
    .execute(&state.pool)
    .await?;
    Ok(HttpResponse::NoContent().finish())
}
pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/api/preferences")
            .wrap(actix_web::middleware::from_fn(crate::auth::protected))
            .route("", web::get().to(get))
            .route("", web::put().to(save))
            .route("/views", web::get().to(views))
            .route("/views/{id}", web::get().to(view))
            .route("/views/{id}", web::put().to(save_view))
            .route("/sound", web::get().to(sound))
            .route("/sound", web::put().to(upload))
            .route("/sound", web::delete().to(clear)),
    );
}

#[derive(Serialize, Deserialize)]
pub struct Viewport {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}
#[derive(Serialize, Deserialize)]
pub struct MapView {
    pub viewport: Viewport,
    pub height: f64,
    pub focus: bool,
    pub device_list: bool,
    pub noc: bool,
}
async fn view(
    req: HttpRequest,
    state: web::Data<AppState>,
    id: web::Path<i64>,
) -> Result<HttpResponse, AppError> {
    let saved: Option<String> =
        sqlx::query_scalar("SELECT view_json FROM user_map_views WHERE user_id=? AND map_id=?")
            .bind(actor(&req)?.id)
            .bind(*id)
            .fetch_optional(&state.pool)
            .await?;
    let value = saved
        .map(|s| serde_json::from_str::<MapView>(&s))
        .transpose()
        .map_err(|_| AppError::Validation("Invalid saved map view".into()))?;
    Ok(HttpResponse::Ok().json(value))
}
async fn views(req: HttpRequest, state: web::Data<AppState>) -> Result<HttpResponse, AppError> {
    let ids: Vec<i64> = sqlx::query_scalar(
        "SELECT map_id FROM user_map_views WHERE user_id=? ORDER BY updated_at DESC,rowid DESC",
    )
    .bind(actor(&req)?.id)
    .fetch_all(&state.pool)
    .await?;
    Ok(HttpResponse::Ok().json(ids))
}
async fn save_view(
    req: HttpRequest,
    state: web::Data<AppState>,
    id: web::Path<i64>,
    input: web::Json<MapView>,
) -> Result<HttpResponse, AppError> {
    let v = &input.viewport;
    if [v.x, v.y, v.w, v.h, input.height]
        .iter()
        .any(|x| !x.is_finite())
        || v.x.abs() > 1e7
        || v.y.abs() > 1e7
        || !(100.0..=200000.0).contains(&v.w)
        || !(100.0..=200000.0).contains(&v.h)
        || !(400.0..=6000.0).contains(&input.height)
    {
        return Err(AppError::Validation("Invalid map view".into()));
    }
    let exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM topology_maps WHERE id=?")
        .bind(*id)
        .fetch_one(&state.pool)
        .await?;
    if exists == 0 {
        return Err(AppError::NotFound);
    }
    sqlx::query("INSERT INTO user_map_views(user_id,map_id,view_json,updated_at) VALUES (?,?,?,CAST((julianday('now')-2440587.5)*86400000 AS INTEGER)) ON CONFLICT(user_id,map_id) DO UPDATE SET view_json=excluded.view_json,updated_at=excluded.updated_at").bind(actor(&req)?.id).bind(*id).bind(serde_json::to_string(&input.into_inner()).map_err(|_|AppError::Validation("Invalid view".into()))?).execute(&state.pool).await?;
    Ok(HttpResponse::NoContent().finish())
}
