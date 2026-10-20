use super::*;
use actix_web::{
    cookie::{time::Duration, Cookie, SameSite},
    web, HttpResponse,
};
use serde::Deserialize;
#[derive(Deserialize)]
struct Credentials {
    username: String,
    password: String,
    #[serde(default)]
    setup_key: String,
}
fn limit(req: &HttpRequest, state: &AppState) -> Result<(), AppError> {
    state.auth.rate_limit(
        &req.peer_addr()
            .map(|a| a.ip().to_string())
            .unwrap_or_default(),
    )
}
fn cookie(state: &AppState, token: String) -> Cookie<'static> {
    Cookie::build("topology_session", token)
        .path("/")
        .http_only(true)
        .same_site(SameSite::Strict)
        .secure(state.auth.secure_cookie)
        .max_age(Duration::hours(8))
        .finish()
}
async fn status(req: HttpRequest, state: web::Data<AppState>) -> Result<HttpResponse, AppError> {
    let session = state.auth.principal(&req).await.ok().map(|user| Session {
        csrf_token: user.csrf.clone(),
        user,
    });
    Ok(HttpResponse::Ok().json(
        serde_json::json!({"setup_required":state.auth.setup_required().await?,"session":session}),
    ))
}
async fn enter(
    req: HttpRequest,
    state: web::Data<AppState>,
    input: web::Json<Credentials>,
) -> Result<HttpResponse, AppError> {
    limit(&req, &state)?;
    let setup = req.path().ends_with("/setup");
    let result = if setup {
        state
            .auth
            .bootstrap(&input.setup_key, &input.username, input.password.clone())
            .await
    } else {
        state
            .auth
            .login(&input.username, input.password.clone())
            .await
    };
    let action = if setup { "admin setup" } else { "login" };
    let peer = req
        .peer_addr()
        .map(|p| p.ip().to_string())
        .unwrap_or_default();
    match result {
        Ok(user) => {
            crate::activity::record(
                &state.pool,
                &user.username,
                action,
                &format!("Success from {peer}"),
            )
            .await?;
            let (token, session) = state.auth.new_session(user).await?;
            Ok(HttpResponse::Ok()
                .cookie(cookie(&state, token))
                .json(session))
        }
        Err(error) => {
            let username: String = input
                .username
                .chars()
                .filter(|c| !c.is_control())
                .take(100)
                .collect();
            crate::activity::record(
                &state.pool,
                &username,
                action,
                &format!("Failed attempt from {peer}"),
            )
            .await?;
            Err(error)
        }
    }
}
async fn me(req: HttpRequest) -> Result<HttpResponse, AppError> {
    let user = actor(&req)?;
    Ok(HttpResponse::Ok().json(Session {
        csrf_token: user.csrf.clone(),
        user,
    }))
}
async fn logout(req: HttpRequest, state: web::Data<AppState>) -> Result<HttpResponse, AppError> {
    if let Some(c) = req.cookie("topology_session") {
        sqlx::query("DELETE FROM sessions WHERE token_hash=?")
            .bind(token_hash(c.value()))
            .execute(&state.pool)
            .await?;
    }
    let mut c = cookie(&state, String::new());
    c.make_removal();
    Ok(HttpResponse::NoContent().cookie(c).finish())
}
#[derive(Deserialize)]
struct Password {
    current_password: String,
    new_password: String,
}
async fn password(
    req: HttpRequest,
    state: web::Data<AppState>,
    input: web::Json<Password>,
) -> Result<HttpResponse, AppError> {
    limit(&req, &state)?;
    state
        .auth
        .change_password(
            &actor(&req)?,
            input.current_password.clone(),
            input.new_password.clone(),
        )
        .await?;
    Ok(HttpResponse::NoContent().finish())
}
async fn users(state: web::Data<AppState>) -> Result<HttpResponse, AppError> {
    Ok(HttpResponse::Ok().json(state.auth.users().await?))
}
#[derive(Deserialize)]
struct NewUser {
    username: String,
    password: String,
    role: String,
}
async fn add(
    req: HttpRequest,
    state: web::Data<AppState>,
    input: web::Json<NewUser>,
) -> Result<HttpResponse, AppError> {
    crate::activity::describe(
        &req,
        format!("User: {} · role: {}", input.username, input.role),
    );
    Ok(HttpResponse::Created().json(
        state
            .auth
            .add_user(&input.username, input.password.clone(), &input.role)
            .await?,
    ))
}
#[derive(Deserialize)]
struct UpdateUser {
    role: String,
    enabled: bool,
    password: Option<String>,
}
async fn update(
    req: HttpRequest,
    state: web::Data<AppState>,
    id: web::Path<i64>,
    input: web::Json<UpdateUser>,
) -> Result<HttpResponse, AppError> {
    crate::activity::describe(
        &req,
        format!(
            "User #{} · role: {} · enabled: {} · password reset: {}",
            *id,
            input.role,
            input.enabled,
            input.password.as_ref().is_some_and(|s| !s.is_empty())
        ),
    );
    state
        .auth
        .update_user(*id, &input.role, input.enabled, input.password.clone())
        .await?;
    Ok(HttpResponse::NoContent().finish())
}
pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.route("/api/auth/status", web::get().to(status))
        .route("/api/auth/setup", web::post().to(enter))
        .route("/api/auth/login", web::post().to(enter))
        .service(
            web::scope("/api/auth")
                .wrap(actix_web::middleware::from_fn(protected))
                .route("/me", web::get().to(me))
                .route("/logout", web::post().to(logout))
                .route("/password", web::post().to(password))
                .route("/users", web::get().to(users))
                .route("/users", web::post().to(add))
                .route("/users/{id}", web::put().to(update)),
        );
}
