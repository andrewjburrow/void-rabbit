use super::TopologyService;
use crate::error::AppError;
use lettre::{
    message::Mailbox, transport::smtp::authentication::Credentials, AsyncSmtpTransport,
    AsyncTransport, Message, Tokio1Executor,
};
use serde::{Deserialize, Serialize};
use std::time::Duration;
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    pub security: String,
    pub username: String,
    pub password: String,
    pub from: String,
    pub recipients: String,
    pub version: i64,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: false,
            host: String::new(),
            port: 587,
            security: "starttls".into(),
            username: String::new(),
            password: String::new(),
            from: String::new(),
            recipients: String::new(),
            version: 0,
        }
    }
}
#[derive(Deserialize)]
pub struct ConfigInput {
    #[serde(flatten)]
    pub config: Config,
    #[serde(default)]
    pub clear_password: bool,
}
impl Config {
    pub fn addresses(&self) -> Result<Vec<Mailbox>, AppError> {
        let addresses = self
            .recipients
            .split([',', ';', '\n'])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| {
                s.parse::<Mailbox>()
                    .map_err(|_| AppError::Validation(format!("Invalid recipient address: {s}")))
            })
            .collect::<Result<Vec<_>, _>>()?;
        if addresses.is_empty() || addresses.len() > 20 {
            return Err(AppError::Validation(
                "Configure 1–20 alert recipients".into(),
            ));
        }
        Ok(addresses)
    }
    pub fn validate(&self) -> Result<(), AppError> {
        if !["starttls", "tls", "none"].contains(&self.security.as_str())
            || self.port == 0
            || self.host.len() > 253
            || self.username.len() > 200
            || self.password.len() > 1000
            || self.recipients.len() > 2000
        {
            return Err(AppError::Validation("Invalid SMTP settings".into()));
        }
        if self.enabled {
            if self.host.trim().is_empty() || self.host.chars().any(char::is_whitespace) {
                return Err(AppError::Validation("SMTP host is required".into()));
            }
            self.from
                .parse::<Mailbox>()
                .map_err(|_| AppError::Validation("Enter a valid sender email address".into()))?;
            self.addresses()?;
            if self.username.is_empty() != self.password.is_empty() {
                return Err(AppError::Validation(
                    "Provide both SMTP username and password, or neither for a relay".into(),
                ));
            }
        }
        Ok(())
    }
    pub fn public(&self) -> serde_json::Value {
        let mut value = serde_json::to_value(self).expect("SMTP fields serialize");
        value.as_object_mut().unwrap().remove("password");
        value["password_set"] = (!self.password.is_empty()).into();
        value
    }
}
impl TopologyService {
    pub async fn smtp(&self) -> Result<Config, AppError> {
        let data: Option<String> =
            sqlx::query_scalar("SELECT config_json FROM smtp_settings WHERE id=1")
                .fetch_optional(&self.pool)
                .await?;
        data.map(|s| {
            serde_json::from_str(&s)
                .map_err(|_| AppError::Validation("Stored SMTP settings are invalid".into()))
        })
        .unwrap_or(Ok(Config::default()))
    }
    pub async fn save_smtp(&self, mut input: ConfigInput) -> Result<Config, AppError> {
        let mut tx = self.pool.begin().await?;
        let previous: Option<String> =
            sqlx::query_scalar("SELECT config_json FROM smtp_settings WHERE id=1")
                .fetch_optional(&mut *tx)
                .await?;
        let old: Config = previous
            .map(|s| serde_json::from_str(&s))
            .transpose()
            .map_err(|_| AppError::Validation("Stored SMTP settings are invalid".into()))?
            .unwrap_or_default();
        if input.config.version != old.version {
            return Err(AppError::Conflict(
                "SMTP settings changed; reopen them before saving".into(),
            ));
        }
        if input.clear_password {
            input.config.password.clear();
        } else if input.config.password.is_empty() {
            input.config.password = old.password;
        }
        input.config.host = input.config.host.trim().into();
        input.config.from = input.config.from.trim().into();
        input.config.validate()?;
        input.config.version += 1;
        let value = serde_json::to_string(&input.config)
            .map_err(|e| AppError::Validation(e.to_string()))?;
        sqlx::query("INSERT INTO smtp_settings(id,config_json) VALUES(1,?) ON CONFLICT(id) DO UPDATE SET config_json=excluded.config_json").bind(value).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(input.config)
    }
    pub async fn queue_test(&self) -> Result<i64, AppError> {
        let config = self.smtp().await?;
        if !config.enabled {
            return Err(AppError::Validation(
                "Enable and save SMTP before sending a test".into(),
            ));
        }
        config.validate()?;
        let recipients = config
            .addresses()?
            .into_iter()
            .map(|a| a.to_string())
            .collect::<Vec<_>>();
        let recipients =
            serde_json::to_string(&recipients).map_err(|e| AppError::Validation(e.to_string()))?;
        Ok(sqlx::query("INSERT INTO alert_queue(subject,body,recipients) VALUES ('Void Rabbit — test alert','Your Void Rabbit SMTP alert configuration is working.',?)").bind(recipients).execute(&self.pool).await?.last_insert_rowid())
    }
    pub async fn deliveries(&self) -> Result<Vec<serde_json::Value>, AppError> {
        let rows:Vec<(i64,String,String,i64,String,i64)>=sqlx::query_as("SELECT id,subject,state,attempts,last_error,created_at FROM alert_queue ORDER BY id DESC LIMIT 30").fetch_all(&self.pool).await?;
        Ok(rows.into_iter().map(|(id,subject,state,attempts,error,time)|serde_json::json!({"id":id,"subject":subject,"state":state,"attempts":attempts,"error":error,"created_at":time})).collect())
    }
    pub async fn deliver_one(&self) -> Result<bool, AppError> {
        let config = self.smtp().await?;
        if !config.enabled {
            return Ok(false);
        }
        let item:Option<(i64,String,String,String)>=sqlx::query_as("SELECT id,subject,body,recipients FROM alert_queue WHERE state='pending' AND next_attempt<=unixepoch() ORDER BY id LIMIT 1").fetch_optional(&self.pool).await?;
        let Some((id, subject, body, recipients)) = item else {
            return Ok(false);
        };
        let recipients: Vec<String> = serde_json::from_str(&recipients)
            .map_err(|_| AppError::Validation("Invalid queued recipients".into()))?;
        let outcome = send(&config, &subject, &body, &recipients).await;
        match outcome {
            Ok(()) => {
                crate::activity::record(
                    &self.pool,
                    "mailer",
                    "SMTP delivered",
                    &format!("Delivery #{id}: {subject}"),
                )
                .await?;
                sqlx::query("UPDATE alert_queue SET state='sent',attempts=attempts+1,last_error='' WHERE id=?").bind(id).execute(&self.pool).await?;
            }
            Err(error) => {
                crate::activity::record(
                    &self.pool,
                    "mailer",
                    "SMTP delivery failed",
                    &format!(
                        "Delivery #{id}: {subject} · {}",
                        error.chars().take(500).collect::<String>()
                    ),
                )
                .await?;
                sqlx::query("UPDATE alert_queue SET attempts=attempts+1,last_error=?,next_attempt=unixepoch()+120,state=CASE WHEN attempts+1>=5 THEN 'failed' ELSE 'pending' END WHERE id=?").bind(error.chars().take(500).collect::<String>()).bind(id).execute(&self.pool).await?;
            }
        }
        Ok(true)
    }
}
async fn send(
    config: &Config,
    subject: &str,
    body: &str,
    recipients: &[String],
) -> Result<(), String> {
    let from = config.from.parse::<Mailbox>().map_err(|e| e.to_string())?;
    let mut message = Message::builder().from(from).subject(subject);
    for recipient in recipients {
        message = message.to(recipient.parse::<Mailbox>().map_err(|e| e.to_string())?);
    }
    let message = message.body(body.to_owned()).map_err(|e| e.to_string())?;
    let builder = match config.security.as_str() {
        "tls" => {
            AsyncSmtpTransport::<Tokio1Executor>::relay(&config.host).map_err(|e| e.to_string())?
        }
        "starttls" => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&config.host)
            .map_err(|e| e.to_string())?,
        "none" => AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&config.host),
        _ => return Err("Invalid SMTP security mode".into()),
    };
    let mut builder = builder
        .port(config.port)
        .timeout(Some(Duration::from_secs(10)));
    if !config.username.is_empty() {
        builder = builder.credentials(Credentials::new(
            config.username.clone(),
            config.password.clone(),
        ));
    }
    tokio::time::timeout(Duration::from_secs(30), builder.build().send(message))
        .await
        .map_err(|_| "SMTP delivery timed out".to_string())?
        .map_err(|e| e.to_string())?;
    Ok(())
}
