use crate::{auth::actor, error::AppError, AppState};
use actix_web::{web, HttpRequest, HttpResponse};
use futures_core::Stream;
use std::{
    path::PathBuf,
    pin::Pin,
    task::{Context, Poll},
};
use tokio_util::io::ReaderStream;
struct BackupStream {
    reader: Option<ReaderStream<tokio::fs::File>>,
    directory: PathBuf,
}
impl Stream for BackupStream {
    type Item = Result<web::Bytes, std::io::Error>;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(
            self.get_mut()
                .reader
                .as_mut()
                .expect("Reader remains until drop"),
        )
        .poll_next(cx)
    }
}
impl Drop for BackupStream {
    fn drop(&mut self) {
        drop(self.reader.take());
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}
fn io_error(error: std::io::Error) -> AppError {
    eprintln!("Backup I/O: {error}");
    AppError::Validation("Unable to create backup; check server disk space and permissions".into())
}
pub async fn snapshot(
    pool: &sqlx::SqlitePool,
    destination: &std::path::Path,
) -> Result<(), AppError> {
    let name = destination
        .to_str()
        .ok_or_else(|| AppError::Validation("Backup path must be valid Unicode".into()))?;
    sqlx::query("VACUUM INTO ?")
        .bind(name)
        .execute(pool)
        .await?;
    Ok(())
}
async fn download(req: HttpRequest, state: web::Data<AppState>) -> Result<HttpResponse, AppError> {
    let user = actor(&req)?;
    crate::activity::record(
        &state.pool,
        &user.username,
        "Database backup",
        "Consistent SQLite snapshot requested",
    )
    .await?;
    let directory = std::env::temp_dir().join(format!(
        "void-rabbit-backup-{}",
        crate::auth::random_secret()
    ));
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(&directory).map_err(io_error)?;
    let file = directory.join("backup.sqlite");
    if let Err(error) = snapshot(&state.pool, &file).await {
        let _ = std::fs::remove_dir_all(&directory);
        return Err(error);
    }
    let opened = match tokio::fs::File::open(&file).await {
        Ok(file) => file,
        Err(error) => {
            let _ = std::fs::remove_dir_all(&directory);
            return Err(io_error(error));
        }
    };
    let stream = BackupStream {
        reader: Some(ReaderStream::new(opened)),
        directory,
    };
    Ok(HttpResponse::Ok()
        .content_type("application/vnd.sqlite3")
        .insert_header((
            "Content-Disposition",
            "attachment; filename=void-rabbit-backup.sqlite",
        ))
        .streaming(stream))
}
pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/api/backup")
            .wrap(actix_web::middleware::from_fn(crate::auth::protected))
            .route("", web::get().to(download)),
    );
}
pub async fn recover_admin(
    auth: &crate::auth::AuthService,
    username: &str,
    password: String,
) -> Result<(), AppError> {
    let row: Option<(i64, String)> = sqlx::query_as("SELECT id,role FROM users WHERE username=?")
        .bind(username.trim())
        .fetch_optional(&auth.pool)
        .await?;
    let (id, role) = row.ok_or(AppError::NotFound)?;
    if role != "admin" {
        return Err(AppError::Validation(
            "Recovery is only for an existing admin account".into(),
        ));
    }
    auth.update_user(id, "admin", true, Some(password)).await?;
    crate::activity::record(
        &auth.pool,
        "local console",
        "Admin recovery",
        &format!(
            "Account {} password reset; sessions revoked",
            username.trim()
        ),
    )
    .await?;
    Ok(())
}
