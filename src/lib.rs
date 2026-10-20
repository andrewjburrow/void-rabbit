pub mod activity;
pub mod auth;
pub mod backup;
pub mod error;
pub mod preferences;
pub mod topology;
use actix_web::{web, HttpResponse};
pub struct AppState {
    pub pool: sqlx::SqlitePool,
    pub auth: auth::AuthService,
}
async fn logo() -> HttpResponse {
    HttpResponse::Ok()
        .content_type("image/png")
        .body(&include_bytes!("../static/void-rabbit-logo.png")[..])
}
async fn index() -> HttpResponse {
    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(include_str!("../static/index.html"))
}
async fn css() -> HttpResponse {
    HttpResponse::Ok()
        .content_type("text/css; charset=utf-8")
        .body(include_str!("../static/style.css"))
}
async fn js() -> HttpResponse {
    HttpResponse::Ok()
        .content_type("text/javascript; charset=utf-8")
        .body(include_str!("../static/app.js"))
}
async fn health() -> HttpResponse {
    HttpResponse::Ok().json(serde_json::json!({"status":"ok"}))
}
pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.app_data(
        web::JsonConfig::default()
            .limit(65536)
            .error_handler(|e, _| error::AppError::Validation(format!("Invalid JSON: {e}")).into()),
    )
    .app_data(web::PayloadConfig::new(5 * 1024 * 1024))
    .configure(auth::routes::configure)
    .configure(topology::routes::configure)
    .configure(preferences::configure)
    .configure(activity::configure)
    .configure(backup::configure)
    .route("/void-rabbit-logo.png", web::get().to(logo))
    .route("/favicon.png", web::get().to(logo))
    .route("/", web::get().to(index))
    .route("/style.css", web::get().to(css))
    .route("/app.js", web::get().to(js))
    .route("/health", web::get().to(health));
}
