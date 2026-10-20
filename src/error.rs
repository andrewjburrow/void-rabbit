use actix_web::{http::StatusCode, HttpResponse, ResponseError};
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("{0}")]
    Validation(String),
    #[error("Resource not found")]
    NotFound,
    #[error("{0}")]
    Conflict(String),
    #[error("Invalid credentials or session expired")]
    Unauthorized,
    #[error("Database operation failed")]
    Database(#[source] sqlx::Error),
    #[error("You do not have permission for this action")]
    Forbidden,
    #[error("Too many attempts; try again in one minute")]
    RateLimited,
}
impl From<sqlx::Error> for AppError {
    fn from(error: sqlx::Error) -> Self {
        if let sqlx::Error::Database(ref db) = error {
            if db.is_unique_violation() {
                return Self::Conflict("That name already exists".into());
            }
        }
        eprintln!("Database error: {error}");
        Self::Database(error)
    }
}
impl ResponseError for AppError {
    fn status_code(&self) -> StatusCode {
        match self {
            Self::Validation(_) => StatusCode::BAD_REQUEST,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::Forbidden => StatusCode::FORBIDDEN,
            Self::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
    fn error_response(&self) -> HttpResponse {
        HttpResponse::build(self.status_code()).json(serde_json::json!({"error": self.to_string()}))
    }
}
