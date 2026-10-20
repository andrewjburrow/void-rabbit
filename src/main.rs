use actix_web::{middleware, web, App, HttpServer};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use std::{env, path::PathBuf, time::Duration};
use void_rabbit::{configure, AppState};

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let arguments: Vec<String> = env::args().skip(1).collect();
    let recovery = arguments.first().is_some_and(|s| s == "recover-admin");
    if !arguments.is_empty() && (!recovery || arguments.len() != 2) {
        return Err(std::io::Error::other(
            "Usage: void-rabbit [recover-admin USERNAME]",
        ));
    }
    let bind = env::var("TOPOLOGY_BIND")
        .or_else(|_| env::var("ITAM_BIND"))
        .unwrap_or_else(|_| "0.0.0.0:8080".into());
    let db_path = PathBuf::from(
        env::var("TOPOLOGY_DB_PATH")
            .or_else(|_| env::var("ITAM_DB_PATH"))
            .unwrap_or_else(|_| "data/itam.sqlite".into()),
    );
    if let Some(parent) = db_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(
            SqliteConnectOptions::new()
                .filename(&db_path)
                .create_if_missing(!recovery)
                .journal_mode(SqliteJournalMode::Wal)
                .busy_timeout(Duration::from_secs(10)),
        )
        .await
        .map_err(std::io::Error::other)?;
    let topology = void_rabbit::topology::TopologyService::new(pool.clone());
    topology.initialize().await.map_err(std::io::Error::other)?;
    void_rabbit::activity::initialize(&pool)
        .await
        .map_err(std::io::Error::other)?;
    void_rabbit::preferences::initialize(&pool)
        .await
        .map_err(std::io::Error::other)?;
    let auth = void_rabbit::auth::AuthService::initialize(
        pool.clone(),
        env::var("TOPOLOGY_SECURE_COOKIE").is_ok_and(|s| s == "true"),
    )
    .await
    .map_err(std::io::Error::other)?;
    if recovery {
        let password = rpassword::prompt_password("New admin password: ")?;
        let confirmation = rpassword::prompt_password("Confirm password: ")?;
        if password != confirmation {
            return Err(std::io::Error::other("Passwords do not match"));
        }
        void_rabbit::backup::recover_admin(&auth, &arguments[1], password)
            .await
            .map_err(std::io::Error::other)?;
        eprintln!("Admin password updated. Existing sessions revoked.");
        pool.close().await;
        return Ok(());
    }
    eprintln!(
        "Void Rabbit listening on {bind}; database: {}",
        db_path.display()
    );
    if auth.setup_required().await.map_err(std::io::Error::other)? {
        eprintln!("INITIAL ADMIN SETUP KEY: {}", auth.setup_key);
    }
    sqlx::query("UPDATE topology_nodes SET next_check=0 WHERE enabled=1")
        .execute(&pool)
        .await
        .map_err(std::io::Error::other)?;
    void_rabbit::topology::monitor::start(topology);
    let state = web::Data::new(AppState { pool, auth });
    HttpServer::new(move || App::new().app_data(state.clone())
        .wrap(middleware::DefaultHeaders::new()
            .add(("X-Content-Type-Options", "nosniff"))
            .add(("Referrer-Policy", "same-origin"))
            .add(("Cache-Control", "no-store"))
            .add(("Content-Security-Policy", "default-src 'self'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; media-src 'self' blob:;  frame-ancestors 'none'; base-uri 'none'; form-action 'self'")))
        .configure(configure))
        .workers(2).bind(&bind)?.run().await
}
