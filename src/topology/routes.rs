use super::{smtp::ConfigInput, DeviceInput, GroupInput, TopologyService};
use crate::{auth::actor, error::AppError, AppState};
use actix_web::{web, HttpRequest, HttpResponse};
use serde::Deserialize;
fn service(req: &HttpRequest, state: &AppState) -> Result<TopologyService, AppError> {
    actor(req)?;
    Ok(TopologyService::new(state.pool.clone()))
}
#[derive(Deserialize)]
struct Name {
    name: String,
}
#[derive(Deserialize)]
struct Xml {
    xml: String,
}
#[derive(Deserialize)]
struct Revision {
    version: i64,
}
async fn maps(req: HttpRequest, state: web::Data<AppState>) -> Result<HttpResponse, AppError> {
    Ok(HttpResponse::Ok().json(service(&req, &state)?.maps().await?))
}
async fn create(
    req: HttpRequest,
    state: web::Data<AppState>,
    input: web::Json<Name>,
) -> Result<HttpResponse, AppError> {
    crate::activity::describe(&req, format!("Map: {}", input.name));
    Ok(HttpResponse::Created()
        .json(serde_json::json!({"id":service(&req,&state)?.create_map(&input.name).await?})))
}
async fn import(
    req: HttpRequest,
    state: web::Data<AppState>,
    input: web::Json<Xml>,
) -> Result<HttpResponse, AppError> {
    crate::activity::describe(&req, "draw.io upload".into());
    Ok(HttpResponse::Created()
        .json(serde_json::json!({"ids":service(&req,&state)?.import(&input.xml).await?})))
}
async fn map(
    req: HttpRequest,
    state: web::Data<AppState>,
    id: web::Path<i64>,
) -> Result<HttpResponse, AppError> {
    Ok(HttpResponse::Ok().json(service(&req, &state)?.map(*id).await?))
}
async fn delete_map(
    req: HttpRequest,
    state: web::Data<AppState>,
    id: web::Path<i64>,
) -> Result<HttpResponse, AppError> {
    let name: Option<String> = sqlx::query_scalar("SELECT name FROM topology_maps WHERE id=?")
        .bind(*id)
        .fetch_optional(&state.pool)
        .await?;
    crate::activity::describe(&req, format!("Map #{}: {}", *id, name.unwrap_or_default()));
    service(&req, &state)?.delete_map(*id).await?;
    Ok(HttpResponse::NoContent().finish())
}
async fn node_create(
    req: HttpRequest,
    state: web::Data<AppState>,
    map: web::Path<i64>,
    input: web::Json<DeviceInput>,
) -> Result<HttpResponse, AppError> {
    crate::activity::describe(
        &req,
        format!(
            "Device: {} · IP: {} · group #{} · enabled: {} · position: ({:.0}, {:.0})",
            input.label, input.ip, input.group_id, input.enabled, input.x, input.y
        ),
    );
    Ok(HttpResponse::Created().json(
        service(&req, &state)?
            .save_device(Some(*map), None, input.into_inner())
            .await?,
    ))
}
async fn node_update(
    req: HttpRequest,
    state: web::Data<AppState>,
    id: web::Path<i64>,
    input: web::Json<DeviceInput>,
) -> Result<HttpResponse, AppError> {
    crate::activity::describe(
        &req,
        format!(
            "Device: {} · IP: {} · group #{} · enabled: {} · position: ({:.0}, {:.0})",
            input.label, input.ip, input.group_id, input.enabled, input.x, input.y
        ),
    );
    Ok(HttpResponse::Ok().json(
        service(&req, &state)?
            .save_device(None, Some(*id), input.into_inner())
            .await?,
    ))
}
async fn node_delete(
    req: HttpRequest,
    state: web::Data<AppState>,
    id: web::Path<i64>,
    rev: web::Query<Revision>,
) -> Result<HttpResponse, AppError> {
    let node: Option<(String, String)> =
        sqlx::query_as("SELECT label,ip FROM topology_nodes WHERE id=?")
            .bind(*id)
            .fetch_optional(&state.pool)
            .await?;
    crate::activity::describe(
        &req,
        format!(
            "Device #{}: {}",
            *id,
            node.map(|(label, ip)| format!("{label} ({ip})"))
                .unwrap_or_default()
        ),
    );
    service(&req, &state)?
        .delete_device(*id, rev.version)
        .await?;
    Ok(HttpResponse::NoContent().finish())
}
async fn groups(req: HttpRequest, state: web::Data<AppState>) -> Result<HttpResponse, AppError> {
    Ok(HttpResponse::Ok().json(service(&req, &state)?.groups().await?))
}
async fn group_create(
    req: HttpRequest,
    state: web::Data<AppState>,
    input: web::Json<GroupInput>,
) -> Result<HttpResponse, AppError> {
    crate::activity::describe(
        &req,
        format!(
            "Group: {} · interval: {} min · misses before red: {} · enabled: {} · SMTP alerts: {}",
            input.name, input.interval_min, input.failure_threshold, input.enabled, input.alerts
        ),
    );
    Ok(HttpResponse::Created().json(
        service(&req, &state)?
            .save_group(None, input.into_inner())
            .await?,
    ))
}
async fn group_update(
    req: HttpRequest,
    state: web::Data<AppState>,
    id: web::Path<i64>,
    input: web::Json<GroupInput>,
) -> Result<HttpResponse, AppError> {
    crate::activity::describe(
        &req,
        format!(
            "Group: {} · interval: {} min · misses before red: {} · enabled: {} · SMTP alerts: {}",
            input.name, input.interval_min, input.failure_threshold, input.enabled, input.alerts
        ),
    );
    Ok(HttpResponse::Ok().json(
        service(&req, &state)?
            .save_group(Some(*id), input.into_inner())
            .await?,
    ))
}
#[derive(Deserialize)]
struct Edges {
    version: i64,
    edges: Vec<super::import::Edge>,
}
async fn edges(
    req: HttpRequest,
    state: web::Data<AppState>,
    id: web::Path<i64>,
    input: web::Json<Edges>,
) -> Result<HttpResponse, AppError> {
    crate::activity::describe(
        &req,
        format!("Map #{} · {} connections", *id, input.edges.len()),
    );
    service(&req, &state)?
        .save_edges(*id, input.version, input.edges.clone())
        .await?;
    Ok(HttpResponse::NoContent().finish())
}
async fn check(
    req: HttpRequest,
    state: web::Data<AppState>,
    id: web::Path<i64>,
) -> Result<HttpResponse, AppError> {
    crate::activity::describe(&req, format!("Map #{}", *id));
    service(&req, &state)?.check_now(*id).await?;
    Ok(HttpResponse::Accepted().json(serde_json::json!({"queued":true})))
}
async fn events(req: HttpRequest, state: web::Data<AppState>) -> Result<HttpResponse, AppError> {
    Ok(HttpResponse::Ok().json(service(&req, &state)?.events().await?))
}
async fn smtp(req: HttpRequest, state: web::Data<AppState>) -> Result<HttpResponse, AppError> {
    Ok(HttpResponse::Ok().json(service(&req, &state)?.smtp().await?.public()))
}
async fn smtp_save(
    req: HttpRequest,
    state: web::Data<AppState>,
    input: web::Json<ConfigInput>,
) -> Result<HttpResponse, AppError> {
    crate::activity::describe(
        &req,
        "SMTP settings saved (credentials omitted)".to_string(),
    );
    Ok(HttpResponse::Ok().json(
        service(&req, &state)?
            .save_smtp(input.into_inner())
            .await?
            .public(),
    ))
}
async fn smtp_test(req: HttpRequest, state: web::Data<AppState>) -> Result<HttpResponse, AppError> {
    Ok(HttpResponse::Accepted()
        .json(serde_json::json!({"id":service(&req,&state)?.queue_test().await?})))
}
async fn deliveries(
    req: HttpRequest,
    state: web::Data<AppState>,
) -> Result<HttpResponse, AppError> {
    Ok(HttpResponse::Ok().json(service(&req, &state)?.deliveries().await?))
}
async fn js() -> HttpResponse {
    HttpResponse::Ok()
        .content_type("text/javascript; charset=utf-8")
        .body(include_str!("../../static/topology.js"))
}
async fn css() -> HttpResponse {
    HttpResponse::Ok()
        .content_type("text/css; charset=utf-8")
        .body(include_str!("../../static/topology.css"))
}
pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.route("/topology.js", web::get().to(js))
        .route("/topology.css", web::get().to(css))
        .service(
            web::scope("/api/topology")
                .wrap(actix_web::middleware::from_fn(crate::auth::protected))
                .app_data(
                    web::JsonConfig::default()
                        .limit(3 * 1024 * 1024)
                        .error_handler(|e, _| {
                            AppError::Validation(format!("Invalid topology JSON: {e}")).into()
                        }),
                )
                .route("/diagnostics/ping", web::post().to(diagnostic))
                .route("/maps", web::get().to(maps))
                .route("/maps", web::post().to(create))
                .route("/import", web::post().to(import))
                .route("/maps/{id}", web::get().to(map))
                .route("/maps/{id}", web::delete().to(delete_map))
                .route("/maps/{id}/nodes", web::post().to(node_create))
                .route("/maps/{id}/edges", web::put().to(edges))
                .route("/maps/{id}/check", web::post().to(check))
                .route("/maps/{id}/undo", web::post().to(undo))
                .route("/nodes/{id}/outages", web::get().to(outages))
                .route("/nodes/{id}", web::put().to(node_update))
                .route("/nodes/{id}", web::delete().to(node_delete))
                .route("/groups", web::get().to(groups))
                .route("/groups", web::post().to(group_create))
                .route("/groups/{id}", web::put().to(group_update))
                .route("/events", web::get().to(events))
                .route("/smtp", web::get().to(smtp))
                .route("/smtp", web::put().to(smtp_save))
                .route("/smtp/test", web::post().to(smtp_test))
                .route("/deliveries", web::get().to(deliveries)),
        );
}

#[derive(Deserialize)]
struct PingInput {
    ip: String,
}
async fn diagnostic(
    req: HttpRequest,
    input: web::Json<PingInput>,
) -> Result<HttpResponse, AppError> {
    let ip: std::net::IpAddr = input
        .ip
        .parse()
        .map_err(|_| AppError::Validation("Enter a valid IPv4 or IPv6 address".into()))?;
    crate::activity::describe(&req, format!("IP: {}", input.ip));
    let probe = super::monitor::ping(&ip.to_string()).await;
    let (status, latency, detail) = match probe {
        super::monitor::Probe::Up(v) => ("up", Some(v), "Reply received".to_string()),
        super::monitor::Probe::Down => ("down", None, "No echo reply received".to_string()),
        super::monitor::Probe::Error(e) => ("error", None, e),
    };
    Ok(HttpResponse::Ok().json(serde_json::json!({"server_os":std::env::consts::OS,"ip":ip.to_string(),"status":status,"latency_ms":latency,"detail":detail})))
}

#[derive(Deserialize)]
struct UndoInput {
    edit_revision: i64,
}
async fn undo(
    req: HttpRequest,
    state: web::Data<AppState>,
    id: web::Path<i64>,
    input: web::Json<UndoInput>,
) -> Result<HttpResponse, AppError> {
    crate::activity::describe(&req, format!("Map #{}", *id));
    service(&req, &state)?
        .undo(*id, input.edit_revision)
        .await?;
    Ok(HttpResponse::NoContent().finish())
}
async fn outages(
    req: HttpRequest,
    state: web::Data<AppState>,
    id: web::Path<i64>,
) -> Result<HttpResponse, AppError> {
    Ok(HttpResponse::Ok().json(service(&req, &state)?.outages(*id).await?))
}
