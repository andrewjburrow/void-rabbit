use super::{smtp::Config, Device, TopologyService};
use crate::error::AppError;
use std::{
    net::IpAddr,
    time::{Duration, Instant},
};
use tokio::{process::Command, task::JoinSet};
#[derive(Debug)]
pub enum Probe {
    Up(f64),
    Down,
    Error(String),
}
pub fn ping_arguments(ip: IpAddr, windows: bool) -> Vec<String> {
    let mut args = vec![if ip.is_ipv6() { "-6" } else { "-4" }.to_string()];
    let options = if windows {
        vec!["-n", "1", "-w", "2000"]
    } else {
        vec!["-n", "-c", "1", "-W", "2"]
    };
    args.extend(options.into_iter().map(String::from));
    args.push(ip.to_string());
    args
}
pub fn latency(text: &str) -> Option<f64> {
    let lower = text.to_lowercase();
    for marker in ["time=", "time<"] {
        if let Some(part) = lower.split(marker).nth(1) {
            let value: String = part
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.')
                .collect();
            if let Ok(v) = value.parse::<f64>() {
                return Some(if marker == "time<" { v / 2.0 } else { v });
            }
        }
    }
    // Localized Windows output may translate the name of the time field.
    for unit in ["ms", "мс", "毫秒"] {
        if let Some(prefix) = lower.split(unit).next().filter(|_| lower.contains(unit)) {
            let trimmed = prefix.trim_end();
            let numeric: String = trimmed
                .chars()
                .rev()
                .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == ',')
                .collect::<String>()
                .chars()
                .rev()
                .collect();
            if let Ok(value) = numeric.replace(',', ".").parse::<f64>() {
                let before = &trimmed[..trimmed.len() - numeric.len()];
                return Some(if before.ends_with('<') {
                    value / 2.0
                } else {
                    value
                });
            }
        }
    }
    None
}
pub fn parse_output(
    success: bool,
    code: Option<i32>,
    stdout: &str,
    stderr: &str,
    windows: bool,
    elapsed: f64,
) -> Probe {
    let lower = stdout.to_lowercase();
    if success {
        // Windows can exit zero when a router reports 'destination unreachable'.
        let echo = !windows
            || lower.lines().any(|line| {
                line.contains("ttl=")
                    || line.contains("time=")
                    || line.contains("time<")
                    || (line.matches(':').count() >= 2
                        && (line.contains('=') || line.contains('<'))
                        && line.chars().any(|c| c.is_ascii_digit()))
            });
        if echo {
            return Probe::Up(latency(stdout).unwrap_or(elapsed));
        }
        return Probe::Down;
    }
    let combined = format!("{stdout} {stderr}").to_lowercase();
    if windows
        && [
            "general failure",
            "transmit failed",
            "bad value for option",
            "access is denied",
            "usage: ping",
        ]
        .iter()
        .any(|text| combined.contains(text))
    {
        return Probe::Error(format!(
            "Ping utility error: {}",
            format!("{stdout} {stderr}")
                .trim()
                .chars()
                .take(300)
                .collect::<String>()
        ));
    }
    if code == Some(1) {
        return Probe::Down;
    }
    let detail = if stderr.trim().is_empty() {
        stdout
    } else {
        stderr
    };
    Probe::Error(format!(
        "Ping utility failed (exit {:?}): {}",
        code,
        detail.chars().take(300).collect::<String>()
    ))
}
pub async fn ping(ip: &str) -> Probe {
    let Ok(ip) = ip.parse::<IpAddr>() else {
        return Probe::Error("Invalid IP address".into());
    };
    let executable = std::env::var("TOPOLOGY_PING_PATH").unwrap_or_else(|_| {
        if cfg!(windows) {
            std::env::var("SystemRoot")
                .map(|root| format!("{root}\\System32\\PING.EXE"))
                .unwrap_or_else(|_| "ping.exe".into())
        } else {
            "ping".into()
        }
    });
    let mut command = Command::new(executable);
    command
        .kill_on_drop(true)
        .env("LC_ALL", "C")
        .args(ping_arguments(ip, cfg!(windows)));
    let start = Instant::now();
    match tokio::time::timeout(Duration::from_secs(4),command.output()).await{
  Ok(Ok(output))=>parse_output(output.status.success(),output.status.code(),&String::from_utf8_lossy(&output.stdout),&String::from_utf8_lossy(&output.stderr),cfg!(windows),start.elapsed().as_secs_f64()*1000.0),
  Ok(Err(error))=>Probe::Error(format!("Unable to run ping: {error}. Windows uses System32/PING.EXE; Linux requires iputils-ping and ICMP permissions.")),
  Err(_)=>Probe::Error("Ping utility exceeded its four-second execution limit".into()),
 }
}
impl TopologyService {
    pub async fn due(&self) -> Result<Vec<Device>, AppError> {
        Ok(sqlx::query_as("SELECT n.* FROM topology_nodes n JOIN monitor_groups g ON g.id=n.group_id WHERE n.enabled=1 AND g.enabled=1 AND n.ip<>'' AND n.next_check<=unixepoch() ORDER BY n.next_check,n.id LIMIT 16").fetch_all(&self.pool).await?)
    }
    pub async fn apply_probe(&self, id: i64, version: i64, probe: Probe) -> Result<(), AppError> {
        let mut tx = self.pool.begin().await?;
        let node:Option<Device>=sqlx::query_as("SELECT n.* FROM topology_nodes n JOIN monitor_groups g ON g.id=n.group_id WHERE n.id=? AND n.version=? AND n.enabled=1 AND g.enabled=1").bind(id).bind(version).fetch_optional(&mut *tx).await?;
        let Some(node) = node else {
            return Ok(());
        };
        let (interval, threshold, alerts): (i64, i64, bool) = sqlx::query_as(
            "SELECT interval_min,failure_threshold,alerts FROM monitor_groups WHERE id=?",
        )
        .bind(node.group_id)
        .fetch_one(&mut *tx)
        .await?;
        let (status, failures, latency, error) = match probe {
            Probe::Up(latency) => ("up".to_string(), 0, Some(latency), String::new()),
            Probe::Down => {
                let failures = node.failures.saturating_add(1);
                (
                    if failures >= threshold {
                        "down"
                    } else {
                        &node.status
                    }
                    .to_string(),
                    failures,
                    None,
                    String::new(),
                )
            }
            Probe::Error(error) => (node.status.clone(), node.failures, None, error),
        };
        sqlx::query("UPDATE topology_nodes SET status=?,failures=?,latency_ms=?,probe_error=?,last_checked=unixepoch(),last_success_at=CASE WHEN ? THEN unixepoch() ELSE last_success_at END,status_since=CASE WHEN status<>? OR status_since=0 THEN unixepoch() ELSE status_since END,next_check=unixepoch()+? WHERE id=? AND version=?")
   .bind(&status).bind(failures).bind(latency).bind(&error).bind(latency.is_some()).bind(&status).bind(interval*60).bind(id).bind(version).execute(&mut *tx).await?;
        sqlx::query(
            "INSERT INTO activity_log(actor,action,detail) VALUES ('monitor','device check',?)",
        )
        .bind(format!(
            "Device #{} {} ({}): {} · latency {} · consecutive misses {}{}",
            id,
            node.label,
            node.ip,
            if error.is_empty() { &status } else { "error" },
            latency
                .map(|v| format!("{v:.2} ms"))
                .unwrap_or_else(|| "—".into()),
            failures,
            if error.is_empty() {
                String::new()
            } else {
                format!(" · {error}")
            }
        ))
        .execute(&mut *tx)
        .await?;
        if error != node.probe_error {
            sqlx::query("INSERT INTO activity_log(actor,action,detail) VALUES ('monitor','probe diagnostic',?)").bind(format!("Device #{} {} ({}): {}",id,node.label,node.ip,if error.is_empty(){"Probe error cleared"}else{&error})).execute(&mut *tx).await?;
        }
        if status == "down" && node.status != "down" {
            sqlx::query("INSERT INTO device_outages(map_id,device_key,label,ip,started_at) VALUES (?,?,?,?,unixepoch())").bind(node.map_id).bind(&node.cell_id).bind(&node.label).bind(&node.ip).execute(&mut *tx).await?;
        } else if status == "up" {
            sqlx::query("UPDATE device_outages SET ended_at=unixepoch(),end_reason='Recovered' WHERE map_id=? AND device_key=? AND ended_at IS NULL").bind(node.map_id).bind(&node.cell_id).execute(&mut *tx).await?;
        }
        if status != node.status && (status == "up" || status == "down") {
            let message = if status == "up" {
                if node.status == "down" {
                    "Device recovered"
                } else {
                    "Device responded to ping"
                }
            } else {
                "Device did not respond to ping"
            };
            sqlx::query(
                "INSERT INTO network_events(device_id,label,ip,status,message) VALUES (?,?,?,?,?)",
            )
            .bind(id)
            .bind(&node.label)
            .bind(&node.ip)
            .bind(&status)
            .bind(message)
            .execute(&mut *tx)
            .await?;
            sqlx::query("INSERT INTO activity_log(actor,action,detail) VALUES ('monitor','device status',?)").bind(format!("Device #{} {} ({}): {} → {}. {}",id,node.label,node.ip,node.status,status,message)).execute(&mut *tx).await?;
            if alerts && (status == "down" || (status == "up" && node.status == "down")) {
                let config: Option<String> =
                    sqlx::query_scalar("SELECT config_json FROM smtp_settings WHERE id=1")
                        .fetch_optional(&mut *tx)
                        .await?;
                if let Some(config) = config
                    .and_then(|s| serde_json::from_str::<Config>(&s).ok())
                    .filter(|c| c.enabled)
                {
                    let recipients = config
                        .addresses()?
                        .into_iter()
                        .map(|a| a.to_string())
                        .collect::<Vec<_>>();
                    let recipients =
                        serde_json::to_string(&recipients).expect("Address strings serialize");
                    let subject =
                        format!("Void Rabbit — {} — {}", status.to_uppercase(), node.label);
                    let body=format!("{message}\n\nDevice: {}\nIP: {}\nStatus: {}\nConsecutive failed checks: {}\nCheck interval: {} minute(s)\n\nThis status reflects ICMP reachability from the Void Rabbit server.",node.label,node.ip,status,failures,interval);
                    sqlx::query("INSERT INTO alert_queue(subject,body,recipients) VALUES (?,?,?)")
                        .bind(subject)
                        .bind(body)
                        .bind(recipients)
                        .execute(&mut *tx)
                        .await?;
                }
            }
        }
        tx.commit().await?;
        Ok(())
    }
}
pub fn start(service: TopologyService) {
    let mail = service.clone();
    tokio::spawn(async move {
        loop {
            match mail.deliver_one().await {
                Ok(true) => continue,
                Ok(false) => {}
                Err(error) => eprintln!("Alert queue: {error}"),
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });
    tokio::spawn(async move {
        loop {
            match service.due().await {
                Ok(nodes) => {
                    let mut jobs = JoinSet::new();
                    for node in nodes {
                        let service = service.clone();
                        jobs.spawn(async move {
                            let probe = ping(&node.ip).await;
                            service.apply_probe(node.id, node.version, probe).await
                        });
                    }
                    while let Some(result) = jobs.join_next().await {
                        if let Ok(Err(error)) = result {
                            eprintln!("Monitor: {error}");
                        }
                    }
                }
                Err(error) => eprintln!("Monitor scheduler: {error}"),
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });
}
