pub mod routes;
use crate::{error::AppError, AppState};
use actix_web::{
    body::{EitherBody, MessageBody},
    dev::{ServiceRequest, ServiceResponse},
    middleware::Next,
    HttpMessage, HttpRequest, ResponseError,
};
use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use rand::RngCore;
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::Semaphore;

#[derive(Clone)]
pub struct AuthService {
    pub pool: SqlitePool,
    pub setup_key: Arc<String>,
    pub secure_cookie: bool,
    attempts: Arc<Mutex<HashMap<String, (Instant, u32)>>>,
    hash_slots: Arc<Semaphore>,
    dummy_hash: Arc<String>,
}
#[derive(Clone, Serialize, sqlx::FromRow)]
pub struct Principal {
    pub id: i64,
    pub username: String,
    pub role: String,
    #[serde(skip)]
    pub csrf: String,
}
#[derive(Serialize, sqlx::FromRow)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub role: String,
    pub enabled: bool,
    pub created_at: i64,
}
#[derive(Serialize)]
pub struct Session {
    pub user: Principal,
    pub csrf_token: String,
}
#[derive(sqlx::FromRow)]
struct LoginUser {
    id: i64,
    username: String,
    role: String,
    enabled: bool,
    password_hash: String,
}

pub fn random_secret() -> String {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
pub fn token_hash(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}
pub fn secure_eq(a: &str, b: &str) -> bool {
    let mut diff = a.len() ^ b.len();
    for (i, v) in a.bytes().enumerate() {
        diff |= (v ^ b.as_bytes().get(i).copied().unwrap_or(0)) as usize;
    }
    diff == 0
}
pub fn validate_username(username: &str) -> Result<String, AppError> {
    let username = username.trim().to_lowercase();
    if !(3..=32).contains(&username.len())
        || !username
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        return Err(AppError::Validation(
            "Username must be 3–32 letters, numbers, dots, underscores, or hyphens".into(),
        ));
    }
    Ok(username)
}
pub fn validate_password(password: &str) -> Result<(), AppError> {
    if password.chars().count() < 12 || password.len() > 128 {
        return Err(AppError::Validation(
            "Password must be at least 12 characters and at most 128 bytes".into(),
        ));
    }
    Ok(())
}
pub async fn hash_password(password: String) -> Result<String, AppError> {
    tokio::task::spawn_blocking(move || {
        let salt = SaltString::generate(&mut OsRng);
        Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .map(|h| h.to_string())
            .map_err(|_| AppError::Validation("Unable to hash password".into()))
    })
    .await
    .map_err(|_| AppError::Validation("Password worker failed".into()))?
}
impl AuthService {
    pub async fn initialize(pool: SqlitePool, secure_cookie: bool) -> Result<Self, AppError> {
        for query in [
   "CREATE TABLE IF NOT EXISTS users(id INTEGER PRIMARY KEY AUTOINCREMENT,username TEXT NOT NULL UNIQUE COLLATE NOCASE,password_hash TEXT NOT NULL,role TEXT NOT NULL CHECK(role IN ('admin','operator','viewer')),enabled INTEGER NOT NULL DEFAULT 1,created_at INTEGER NOT NULL DEFAULT (unixepoch()))",
   "CREATE TABLE IF NOT EXISTS sessions(token_hash TEXT PRIMARY KEY,user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,csrf TEXT NOT NULL,expires_at INTEGER NOT NULL)",
   "DELETE FROM sessions WHERE expires_at<=unixepoch()",
  ]{sqlx::query(query).execute(&pool).await?;}
        let dummy_hash = hash_password(random_secret()).await?;
        Ok(Self {
            pool,
            setup_key: Arc::new(random_secret()),
            secure_cookie,
            attempts: Arc::new(Mutex::new(HashMap::new())),
            hash_slots: Arc::new(Semaphore::new(4)),
            dummy_hash: Arc::new(dummy_hash),
        })
    }
    pub async fn setup_required(&self) -> Result<bool, AppError> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
            .fetch_one(&self.pool)
            .await?;
        Ok(count == 0)
    }
    pub fn rate_limit(&self, key: &str) -> Result<(), AppError> {
        let mut attempts = self
            .attempts
            .lock()
            .map_err(|_| AppError::Validation("Login rate limiter unavailable".into()))?;
        attempts.retain(|_, (time, _)| time.elapsed() < Duration::from_secs(60));
        if attempts.len() >= 4096 && !attempts.contains_key(key) {
            return Err(AppError::RateLimited);
        }
        let entry = attempts.entry(key.into()).or_insert((Instant::now(), 0));
        entry.1 += 1;
        if entry.1 > 10 {
            return Err(AppError::RateLimited);
        }
        Ok(())
    }
    pub async fn bootstrap(
        &self,
        key: &str,
        username: &str,
        password: String,
    ) -> Result<Principal, AppError> {
        if !secure_eq(&self.setup_key, key) {
            return Err(AppError::Unauthorized);
        }
        let username = validate_username(username)?;
        validate_password(&password)?;
        let _slot = self
            .hash_slots
            .acquire()
            .await
            .map_err(|_| AppError::Unauthorized)?;
        let hashed = hash_password(password).await?;
        let mut tx = self.pool.begin().await?;
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
            .fetch_one(&mut *tx)
            .await?;
        if count != 0 {
            return Err(AppError::Conflict(
                "Initial admin is already configured".into(),
            ));
        }
        let id = sqlx::query("INSERT INTO users(username,password_hash,role) VALUES (?,?,'admin')")
            .bind(&username)
            .bind(hashed)
            .execute(&mut *tx)
            .await?
            .last_insert_rowid();
        tx.commit().await?;
        Ok(Principal {
            id,
            username,
            role: "admin".into(),
            csrf: String::new(),
        })
    }
    pub async fn login(&self, username: &str, password: String) -> Result<Principal, AppError> {
        if username.len() > 100 || password.len() > 128 {
            return Err(AppError::Unauthorized);
        }
        let user: Option<LoginUser> = sqlx::query_as(
            "SELECT id,username,role,enabled,password_hash FROM users WHERE username=?",
        )
        .bind(username.trim())
        .fetch_optional(&self.pool)
        .await?;
        let hash = user
            .as_ref()
            .map(|u| u.password_hash.clone())
            .unwrap_or_else(|| self.dummy_hash.to_string());
        let _slot = self
            .hash_slots
            .acquire()
            .await
            .map_err(|_| AppError::Unauthorized)?;
        let valid = tokio::task::spawn_blocking(move || {
            PasswordHash::new(&hash).is_ok_and(|hash| {
                Argon2::default()
                    .verify_password(password.as_bytes(), &hash)
                    .is_ok()
            })
        })
        .await
        .map_err(|_| AppError::Unauthorized)?;
        let Some(user) = user.filter(|u| u.enabled && valid) else {
            return Err(AppError::Unauthorized);
        };
        Ok(Principal {
            id: user.id,
            username: user.username,
            role: user.role,
            csrf: String::new(),
        })
    }
    pub async fn new_session(&self, mut user: Principal) -> Result<(String, Session), AppError> {
        let token = random_secret();
        let csrf = random_secret();
        user.csrf = csrf.clone();
        sqlx::query("INSERT INTO sessions(token_hash,user_id,csrf,expires_at) VALUES (?,?,?,unixepoch()+28800)").bind(token_hash(&token)).bind(user.id).bind(&csrf).execute(&self.pool).await?;
        sqlx::query("DELETE FROM sessions WHERE expires_at<=unixepoch()")
            .execute(&self.pool)
            .await?;
        Ok((
            token,
            Session {
                user,
                csrf_token: csrf,
            },
        ))
    }
    pub async fn principal(&self, req: &HttpRequest) -> Result<Principal, AppError> {
        let token = req
            .cookie("topology_session")
            .ok_or(AppError::Unauthorized)?
            .value()
            .to_string();
        if token.len() != 64 {
            return Err(AppError::Unauthorized);
        }
        sqlx::query_as("SELECT u.id,u.username,u.role,s.csrf FROM users u JOIN sessions s ON s.user_id=u.id WHERE s.token_hash=? AND s.expires_at>unixepoch() AND u.enabled=1")
   .bind(token_hash(&token)).fetch_optional(&self.pool).await?.ok_or(AppError::Unauthorized)
    }
    pub async fn users(&self) -> Result<Vec<User>, AppError> {
        Ok(
            sqlx::query_as("SELECT id,username,role,enabled,created_at FROM users ORDER BY id")
                .fetch_all(&self.pool)
                .await?,
        )
    }
    pub async fn add_user(
        &self,
        username: &str,
        password: String,
        role: &str,
    ) -> Result<User, AppError> {
        let username = validate_username(username)?;
        validate_password(&password)?;
        if !["admin", "operator", "viewer"].contains(&role) {
            return Err(AppError::Validation("Invalid account role".into()));
        }
        let _slot = self
            .hash_slots
            .acquire()
            .await
            .map_err(|_| AppError::Unauthorized)?;
        let hash = hash_password(password).await?;
        Ok(sqlx::query_as("INSERT INTO users(username,password_hash,role) VALUES (?,?,?) RETURNING id,username,role,enabled,created_at").bind(username).bind(hash).bind(role).fetch_one(&self.pool).await?)
    }
    pub async fn update_user(
        &self,
        id: i64,
        role: &str,
        enabled: bool,
        password: Option<String>,
    ) -> Result<(), AppError> {
        if !["admin", "operator", "viewer"].contains(&role) {
            return Err(AppError::Validation("Invalid account role".into()));
        }
        let hash = if let Some(password) = password.filter(|s| !s.is_empty()) {
            validate_password(&password)?;
            let _slot = self
                .hash_slots
                .acquire()
                .await
                .map_err(|_| AppError::Unauthorized)?;
            Some(hash_password(password).await?)
        } else {
            None
        };
        let mut tx = self.pool.begin().await?;
        let old: Option<(String, bool)> =
            sqlx::query_as("SELECT role,enabled FROM users WHERE id=?")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
        let old = old.ok_or(AppError::NotFound)?;
        if old.0 == "admin" && old.1 && (role != "admin" || !enabled) {
            let count: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE role='admin' AND enabled=1")
                    .fetch_one(&mut *tx)
                    .await?;
            if count <= 1 {
                return Err(AppError::Validation(
                    "Keep at least one enabled admin account".into(),
                ));
            }
        }
        sqlx::query(
            "UPDATE users SET role=?,enabled=?,password_hash=COALESCE(?,password_hash) WHERE id=?",
        )
        .bind(role)
        .bind(enabled)
        .bind(hash)
        .bind(id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM sessions WHERE user_id=?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }
    pub async fn change_password(
        &self,
        user: &Principal,
        current: String,
        new: String,
    ) -> Result<(), AppError> {
        let verified = self.login(&user.username, current).await?;
        if verified.id != user.id {
            return Err(AppError::Unauthorized);
        }
        self.update_user(user.id, &user.role, true, Some(new)).await
    }
}
pub fn actor(req: &HttpRequest) -> Result<Principal, AppError> {
    req.extensions()
        .get::<Principal>()
        .cloned()
        .ok_or(AppError::Unauthorized)
}
pub async fn protected<B: MessageBody + 'static>(
    req: ServiceRequest,
    next: Next<B>,
) -> Result<ServiceResponse<EitherBody<B>>, actix_web::Error> {
    let state = req.app_data::<actix_web::web::Data<AppState>>().cloned();
    let auth = async {
        let state = state.ok_or(AppError::Unauthorized)?;
        let user = state.auth.principal(req.request()).await?;
        req.extensions_mut().insert(user.clone());
        if !matches!(req.method().as_str(), "GET" | "HEAD") {
            if user.role == "viewer"
                && !req.path().starts_with("/api/preferences")
                && !matches!(req.path(), "/api/auth/logout" | "/api/auth/password")
            {
                return Err(AppError::Forbidden);
            }
            let csrf = req
                .headers()
                .get("X-CSRF-Token")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            if !secure_eq(&user.csrf, csrf) {
                return Err(AppError::Forbidden);
            }
        }
        if (req.path().starts_with("/api/topology/smtp")
            || req.path().ends_with("/deliveries")
            || req.path().starts_with("/api/auth/users")
            || req.path().starts_with("/api/backup"))
            && user.role != "admin"
        {
            return Err(AppError::Forbidden);
        }
        req.extensions_mut().insert(user);
        Ok::<(), AppError>(())
    }
    .await;
    if let Err(error) = auth {
        let user = req.extensions().get::<Principal>().cloned();
        if let (Some(state), Some(user)) = (req.app_data::<actix_web::web::Data<AppState>>(), user)
        {
            crate::activity::record(
                &state.pool,
                &user.username,
                "Access denied",
                &format!(
                    "{} {} · HTTP {}",
                    req.method(),
                    req.path(),
                    error.status_code().as_u16()
                ),
            )
            .await?;
        }
        return Ok(req
            .into_response(error.error_response())
            .map_into_right_body());
    }
    let audit_state = req.app_data::<actix_web::web::Data<AppState>>().cloned();
    let user = req.extensions().get::<Principal>().cloned();
    let method = req.method().to_string();
    let path = req.path().to_string();
    let response = next.call(req).await?;
    if method != "GET" && method != "HEAD" && !path.starts_with("/api/preferences/views/") {
        if let (Some(state), Some(user)) = (audit_state, user) {
            let description = response
                .request()
                .extensions()
                .get::<crate::activity::Description>()
                .map(|d| d.0.clone())
                .unwrap_or_default();
            crate::activity::record(
                &state.pool,
                &user.username,
                &crate::activity::action(&method, &path),
                &format!(
                    "{} · {} {} · HTTP {}",
                    description,
                    method,
                    path,
                    response.status().as_u16()
                ),
            )
            .await?;
        }
    }
    Ok(response.map_into_left_body())
}
