use crate::{error::AppError, AppState};
use actix_web::{web, HttpResponse};
use serde::{Deserialize, Serialize};
pub async fn initialize(pool: &sqlx::SqlitePool) -> Result<(), AppError> {
    sqlx::query("CREATE TABLE IF NOT EXISTS activity_log(id INTEGER PRIMARY KEY AUTOINCREMENT,actor TEXT NOT NULL,action TEXT NOT NULL,detail TEXT NOT NULL,created_at INTEGER NOT NULL DEFAULT(unixepoch()))").execute(pool).await?;
    Ok(())
}
pub async fn record(
    pool: &sqlx::SqlitePool,
    actor: &str,
    action: &str,
    detail: &str,
) -> Result<(), AppError> {
    sqlx::query("INSERT INTO activity_log(actor,action,detail) VALUES (?,?,?)")
        .bind(actor)
        .bind(action)
        .bind(detail)
        .execute(pool)
        .await?;
    Ok(())
}
#[derive(Serialize, sqlx::FromRow)]
struct Event {
    id: i64,
    actor: String,
    action: String,
    detail: String,
    created_at: i64,
}
#[derive(Deserialize)]
struct Page {
    #[serde(default)]
    before: i64,
    #[serde(default)]
    q: String,
}
async fn events(
    state: web::Data<AppState>,
    page: web::Query<Page>,
) -> Result<HttpResponse, AppError> {
    let search = format!("%{}%", page.q.chars().take(100).collect::<String>());
    let rows:Vec<Event>=sqlx::query_as("SELECT * FROM activity_log WHERE (?=0 OR id<?) AND (actor LIKE ? OR action LIKE ? OR detail LIKE ?) ORDER BY id DESC LIMIT 100").bind(page.before).bind(page.before).bind(&search).bind(&search).bind(&search).fetch_all(&state.pool).await?;
    Ok(HttpResponse::Ok().json(rows))
}
pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/api/activity")
            .wrap(actix_web::middleware::from_fn(crate::auth::protected))
            .route("", web::get().to(events)),
    );
}

#[derive(Clone)]
pub struct Description(pub String);
pub fn describe(req: &actix_web::HttpRequest, detail: String) {
    use actix_web::HttpMessage;
    req.extensions_mut()
        .insert(Description(detail.chars().take(2000).collect()));
}
pub fn action(method: &str, path: &str) -> String {
    let name = if path.ends_with("/undo") {
        "Map edit undone"
    } else if path.starts_with("/api/auth/users") {
        if method == "POST" {
            "User created"
        } else {
            "User updated"
        }
    } else if path.ends_with("/logout") {
        "Logout"
    } else if path.ends_with("/password") {
        "Password changed"
    } else if path.starts_with("/api/preferences") {
        "Preferences updated"
    } else if path.ends_with("/diagnostics/ping") {
        "Ping diagnostic"
    } else if path.ends_with("/import") {
        "Diagram imported"
    } else if path.ends_with("/check") {
        "Device checks queued"
    } else if path.ends_with("/edges") {
        "Connections edited"
    } else if path.starts_with("/api/topology/smtp") {
        "SMTP configuration"
    } else if path.contains("/nodes") {
        match method {
            "POST" => "Device created",
            "DELETE" => "Device removed",
            _ => "Device edited",
        }
    } else if path.contains("/groups") {
        if method == "POST" {
            "Group created"
        } else {
            "Group edited"
        }
    } else if path.contains("/maps") {
        if method == "DELETE" {
            "Map removed"
        } else {
            "Map created"
        }
    } else {
        "User action"
    };
    name.into()
}
