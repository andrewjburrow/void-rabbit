pub mod history;
pub mod import;
pub mod monitor;
pub mod routes;
pub mod smtp;
use crate::error::AppError;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

#[derive(Clone)]
pub struct TopologyService {
    pub pool: SqlitePool,
}
#[derive(Serialize, sqlx::FromRow)]
pub struct MapSummary {
    pub id: i64,
    pub name: String,
    pub node_count: i64,
}
#[derive(Serialize, Deserialize, sqlx::FromRow)]
pub struct Device {
    pub id: i64,
    pub map_id: i64,
    pub cell_id: String,
    pub label: String,
    pub kind: String,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub ip: String,
    pub group_id: i64,
    pub enabled: bool,
    pub status: String,
    pub failures: i64,
    pub last_checked: i64,
    pub next_check: i64,
    pub last_success_at: i64,
    pub status_since: i64,
    pub latency_ms: Option<f64>,
    pub probe_error: String,
    pub version: i64,
}
#[derive(Serialize, sqlx::FromRow)]
pub struct Group {
    pub id: i64,
    pub name: String,
    pub interval_min: i64,
    pub failure_threshold: i64,
    pub enabled: bool,
    pub alerts: bool,
    pub version: i64,
}
#[derive(Deserialize)]
pub struct GroupInput {
    pub name: String,
    pub interval_min: i64,
    pub failure_threshold: i64,
    pub enabled: bool,
    pub alerts: bool,
    pub version: Option<i64>,
}
#[derive(Deserialize)]
pub struct DeviceInput {
    pub label: String,
    pub kind: String,
    pub ip: String,
    pub group_id: i64,
    pub enabled: bool,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub version: Option<i64>,
}
#[derive(Serialize, Deserialize)]
pub struct MapDetail {
    pub edit_revision: i64,
    pub undo_label: String,
    pub id: i64,
    pub name: String,
    pub version: i64,
    pub nodes: Vec<Device>,
    pub edges: Vec<import::Edge>,
}
#[derive(Serialize, sqlx::FromRow)]
pub struct NetworkEvent {
    pub id: i64,
    pub device_id: Option<i64>,
    pub label: String,
    pub ip: String,
    pub status: String,
    pub occurred_at: i64,
    pub message: String,
}
fn invalid(s: impl Into<String>) -> AppError {
    AppError::Validation(s.into())
}
impl TopologyService {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
    pub async fn initialize(&self) -> Result<(), AppError> {
        crate::activity::initialize(&self.pool).await?;
        // These tables are separate from the existing inventory and preserve its data.
        for query in [
   "CREATE TABLE IF NOT EXISTS monitor_groups (id INTEGER PRIMARY KEY AUTOINCREMENT,name TEXT NOT NULL UNIQUE,interval_min INTEGER NOT NULL DEFAULT 1 CHECK(interval_min BETWEEN 1 AND 10),failure_threshold INTEGER NOT NULL DEFAULT 1 CHECK(failure_threshold BETWEEN 1 AND 5),enabled INTEGER NOT NULL DEFAULT 1,alerts INTEGER NOT NULL DEFAULT 0,version INTEGER NOT NULL DEFAULT 1)",
   "INSERT OR IGNORE INTO monitor_groups(id,name) VALUES (1,'Default')",
   "CREATE TABLE IF NOT EXISTS topology_maps(id INTEGER PRIMARY KEY AUTOINCREMENT,name TEXT NOT NULL,edges_json TEXT NOT NULL DEFAULT '[]',version INTEGER NOT NULL DEFAULT 1)",
   "CREATE TABLE IF NOT EXISTS topology_nodes(id INTEGER PRIMARY KEY AUTOINCREMENT,map_id INTEGER NOT NULL REFERENCES topology_maps(id) ON DELETE CASCADE,cell_id TEXT NOT NULL,label TEXT NOT NULL,kind TEXT NOT NULL,x REAL NOT NULL,y REAL NOT NULL,width REAL NOT NULL,height REAL NOT NULL,ip TEXT NOT NULL DEFAULT '',group_id INTEGER NOT NULL REFERENCES monitor_groups(id),enabled INTEGER NOT NULL DEFAULT 1,status TEXT NOT NULL DEFAULT 'unknown',failures INTEGER NOT NULL DEFAULT 0,last_checked INTEGER NOT NULL DEFAULT 0,next_check INTEGER NOT NULL DEFAULT 0,latency_ms REAL,probe_error TEXT NOT NULL DEFAULT '',version INTEGER NOT NULL DEFAULT 1,UNIQUE(map_id,cell_id))",
   "CREATE INDEX IF NOT EXISTS topology_nodes_due ON topology_nodes(next_check)",
   "CREATE TABLE IF NOT EXISTS network_events(id INTEGER PRIMARY KEY AUTOINCREMENT,device_id INTEGER REFERENCES topology_nodes(id) ON DELETE SET NULL,label TEXT NOT NULL,ip TEXT NOT NULL,status TEXT NOT NULL,occurred_at INTEGER NOT NULL DEFAULT (unixepoch()),message TEXT NOT NULL)",
   "CREATE TABLE IF NOT EXISTS smtp_settings(id INTEGER PRIMARY KEY CHECK(id=1),config_json TEXT NOT NULL)",
   "CREATE TABLE IF NOT EXISTS alert_queue(id INTEGER PRIMARY KEY AUTOINCREMENT,subject TEXT NOT NULL,body TEXT NOT NULL,recipients TEXT NOT NULL,state TEXT NOT NULL DEFAULT 'pending',attempts INTEGER NOT NULL DEFAULT 0,next_attempt INTEGER NOT NULL DEFAULT 0,last_error TEXT NOT NULL DEFAULT '',created_at INTEGER NOT NULL DEFAULT (unixepoch()))",
  ] {sqlx::query(query).execute(&self.pool).await?;}
        self.initialize_history().await?;
        Ok(())
    }
    pub async fn maps(&self) -> Result<Vec<MapSummary>, AppError> {
        Ok(sqlx::query_as("SELECT m.id,m.name,COUNT(n.id) node_count FROM topology_maps m LEFT JOIN topology_nodes n ON n.map_id=m.id GROUP BY m.id ORDER BY m.id DESC").fetch_all(&self.pool).await?)
    }
    pub async fn map(&self, id: i64) -> Result<MapDetail, AppError> {
        let (name, edges, version, edit_revision): (String, String, i64, i64) = sqlx::query_as(
            "SELECT name,edges_json,version,edit_revision FROM topology_maps WHERE id=?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AppError::NotFound)?;
        let nodes = sqlx::query_as("SELECT * FROM topology_nodes WHERE map_id=? ORDER BY id")
            .bind(id)
            .fetch_all(&self.pool)
            .await?;
        let edges = serde_json::from_str(&edges)
            .map_err(|_| invalid("Stored topology has invalid edges"))?;
        let undo_label: Option<String> = sqlx::query_scalar(
            "SELECT description FROM topology_undo WHERE map_id=? ORDER BY id DESC LIMIT 1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(MapDetail {
            edit_revision,
            undo_label: undo_label.unwrap_or_default(),
            id,
            name,
            version,
            nodes,
            edges,
        })
    }
    pub async fn create_map(&self, name: &str) -> Result<i64, AppError> {
        if name.trim().is_empty() || name.chars().count() > 100 {
            return Err(invalid("Map name must be 1–100 characters"));
        }
        Ok(sqlx::query("INSERT INTO topology_maps(name) VALUES (?)")
            .bind(name.trim())
            .execute(&self.pool)
            .await?
            .last_insert_rowid())
    }
    pub async fn import(&self, xml: &str) -> Result<Vec<i64>, AppError> {
        let pages = import::pages(xml)?;
        let diagrams = pages
            .iter()
            .map(|(name, xml)| import::parse(name, xml))
            .collect::<Result<Vec<_>, _>>()?;
        let mut tx = self.pool.begin().await?;
        let mut ids = Vec::new();
        for diagram in diagrams {
            let edges =
                serde_json::to_string(&diagram.edges).map_err(|e| invalid(e.to_string()))?;
            let id = sqlx::query("INSERT INTO topology_maps(name,edges_json) VALUES (?,?)")
                .bind(&diagram.name)
                .bind(edges)
                .execute(&mut *tx)
                .await?
                .last_insert_rowid();
            for node in diagram.nodes {
                let enabled = !matches!(node.kind.as_str(), "label" | "group");
                sqlx::query("INSERT INTO topology_nodes(map_id,cell_id,label,kind,x,y,width,height,ip,group_id,enabled) VALUES (?,?,?,?,?,?,?,?,?,1,?)")
      .bind(id).bind(node.cell_id).bind(node.label).bind(node.kind).bind(node.x).bind(node.y).bind(node.width).bind(node.height).bind(node.ip).bind(enabled).execute(&mut *tx).await?;
            }
            ids.push(id);
        }
        tx.commit().await?;
        Ok(ids)
    }
    pub async fn delete_map(&self, id: i64) -> Result<(), AppError> {
        if sqlx::query("DELETE FROM topology_maps WHERE id=?")
            .bind(id)
            .execute(&self.pool)
            .await?
            .rows_affected()
            == 0
        {
            return Err(AppError::NotFound);
        }
        Ok(())
    }
    pub async fn groups(&self) -> Result<Vec<Group>, AppError> {
        Ok(sqlx::query_as("SELECT * FROM monitor_groups ORDER BY id")
            .fetch_all(&self.pool)
            .await?)
    }
    pub async fn save_group(&self, id: Option<i64>, input: GroupInput) -> Result<Group, AppError> {
        if input.name.trim().is_empty()
            || input.name.chars().count() > 100
            || !(1..=10).contains(&input.interval_min)
            || !(1..=5).contains(&input.failure_threshold)
        {
            return Err(invalid(
                "Group needs a name, a 1–10 minute interval, and a 1–5 failure threshold",
            ));
        }
        let mut tx = self.pool.begin().await?;
        let group: Option<Group> = if let Some(id) = id {
            let version = input
                .version
                .ok_or_else(|| invalid("Group revision is required"))?;
            sqlx::query_as("UPDATE monitor_groups SET name=?,interval_min=?,failure_threshold=?,enabled=?,alerts=?,version=version+1 WHERE id=? AND version=? RETURNING *")
    .bind(input.name.trim()).bind(input.interval_min).bind(input.failure_threshold).bind(input.enabled).bind(input.alerts).bind(id).bind(version).fetch_optional(&mut *tx).await?
        } else {
            sqlx::query_as("INSERT INTO monitor_groups(name,interval_min,failure_threshold,enabled,alerts) VALUES (?,?,?,?,?) RETURNING *")
    .bind(input.name.trim()).bind(input.interval_min).bind(input.failure_threshold).bind(input.enabled).bind(input.alerts).fetch_optional(&mut *tx).await?
        };
        let group = group.ok_or_else(|| {
            AppError::Conflict("Group changed or was removed; refresh before saving".into())
        })?;
        sqlx::query("UPDATE topology_nodes SET next_check=0 WHERE group_id=?")
            .bind(group.id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(group)
    }
    fn validate_device(input: &mut DeviceInput) -> Result<(), AppError> {
        input.label = input.label.trim().into();
        input.ip = input.ip.trim().into();
        if input.label.is_empty() || input.label.chars().count() > 200 {
            return Err(invalid("Device label must be 1–200 characters"));
        }
        if ![
            "router", "switch", "server", "host", "firewall", "cloud", "label", "group",
        ]
        .contains(&input.kind.as_str())
        {
            return Err(invalid("Unsupported device icon"));
        }
        if !input.ip.is_empty() {
            let ip = input
                .ip
                .parse::<std::net::IpAddr>()
                .map_err(|_| invalid("Enter a valid IPv4 or IPv6 address"))?;
            if ip.is_multicast()
                || ip.is_unspecified()
                || ip == std::net::IpAddr::V4(std::net::Ipv4Addr::BROADCAST)
            {
                return Err(invalid("Use a unicast device IP address"));
            }
            input.ip = ip.to_string();
        }
        if [input.x, input.y, input.width, input.height]
            .iter()
            .any(|v| !v.is_finite() || v.abs() > 100000.0)
            || input.width < 16.0
            || input.height < 16.0
            || input.width > 3000.0
            || input.height > 3000.0
        {
            return Err(invalid("Invalid device geometry"));
        }
        if matches!(input.kind.as_str(), "group" | "label") {
            input.enabled = false;
        }
        Ok(())
    }
    pub async fn save_device(
        &self,
        map_id: Option<i64>,
        id: Option<i64>,
        mut input: DeviceInput,
    ) -> Result<Device, AppError> {
        Self::validate_device(&mut input)?;
        let mut tx = self.pool.begin().await?;
        let group: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM monitor_groups WHERE id=?")
            .bind(input.group_id)
            .fetch_one(&mut *tx)
            .await?;
        if group == 0 {
            return Err(invalid("Monitoring group not found"));
        }
        let editing_map = if let Some(id) = id {
            sqlx::query_scalar::<_, i64>("SELECT map_id FROM topology_nodes WHERE id=?")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(AppError::NotFound)?
        } else {
            map_id.ok_or_else(|| invalid("Map ID required"))?
        };
        Self::snapshot(
            &mut tx,
            editing_map,
            if id.is_some() {
                "device edit"
            } else {
                "add device"
            },
        )
        .await?;
        if let Some(id) = id {
            sqlx::query("UPDATE device_outages SET ended_at=unixepoch(),end_reason='Address changed' WHERE device_key=(SELECT cell_id FROM topology_nodes WHERE id=?) AND map_id=? AND ended_at IS NULL AND (SELECT ip FROM topology_nodes WHERE id=?)<>?").bind(id).bind(editing_map).bind(id).bind(&input.ip).execute(&mut *tx).await?;
        }
        let node = if let Some(id) = id {
            let version = input
                .version
                .ok_or_else(|| invalid("Device revision is required"))?;
            sqlx::query_as::<_,Device>("UPDATE topology_nodes SET label=?,kind=?,group_id=?,enabled=?,x=?,y=?,width=?,height=?,status=CASE WHEN ip<>? THEN 'unknown' ELSE status END,failures=CASE WHEN ip<>? THEN 0 ELSE failures END,last_checked=CASE WHEN ip<>? THEN 0 ELSE last_checked END,latency_ms=CASE WHEN ip<>? THEN NULL ELSE latency_ms END,last_success_at=CASE WHEN ip<>? THEN 0 ELSE last_success_at END,status_since=CASE WHEN ip<>? THEN 0 ELSE status_since END,ip=?,probe_error='',next_check=0,version=version+1 WHERE id=? AND version=? RETURNING *")
    .bind(input.label).bind(input.kind).bind(input.group_id).bind(input.enabled).bind(input.x).bind(input.y).bind(input.width).bind(input.height)
    .bind(&input.ip).bind(&input.ip).bind(&input.ip).bind(&input.ip).bind(&input.ip).bind(&input.ip).bind(&input.ip).bind(id).bind(version).fetch_optional(&mut *tx).await?
        } else {
            let map_id = map_id.ok_or_else(|| invalid("Map ID required"))?;
            let exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM topology_maps WHERE id=?")
                .bind(map_id)
                .fetch_one(&mut *tx)
                .await?;
            if exists == 0 {
                return Err(AppError::NotFound);
            }
            sqlx::query_as::<_,Device>("INSERT INTO topology_nodes(map_id,cell_id,label,kind,ip,group_id,enabled,x,y,width,height) VALUES (?,lower(hex(randomblob(16))),?,?,?,?,?,?,?,?,?) RETURNING *")
    .bind(map_id).bind(input.label).bind(input.kind).bind(input.ip).bind(input.group_id).bind(input.enabled).bind(input.x).bind(input.y).bind(input.width).bind(input.height).fetch_optional(&mut *tx).await?
        };
        let node = node.ok_or_else(|| {
            AppError::Conflict("Device changed or was removed; reopen it before saving".into())
        })?;
        Self::bump_edit(&mut tx, editing_map).await?;
        tx.commit().await?;
        Ok(node)
    }
    pub async fn delete_device(&self, id: i64, version: i64) -> Result<(), AppError> {
        let mut tx = self.pool.begin().await?;
        let (map_id, cell_id): (i64, String) =
            sqlx::query_as("SELECT map_id,cell_id FROM topology_nodes WHERE id=? AND version=?")
                .bind(id)
                .bind(version)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or_else(|| {
                    AppError::Conflict("Device changed or was removed; refresh first".into())
                })?;
        Self::snapshot(&mut tx, map_id, "remove device").await?;
        let edges: String = sqlx::query_scalar("SELECT edges_json FROM topology_maps WHERE id=?")
            .bind(map_id)
            .fetch_one(&mut *tx)
            .await?;
        let mut edges: Vec<import::Edge> =
            serde_json::from_str(&edges).map_err(|e| invalid(e.to_string()))?;
        edges.retain(|e| e.source != cell_id && e.target != cell_id);
        sqlx::query("UPDATE topology_maps SET edges_json=?,version=version+1 WHERE id=?")
            .bind(serde_json::to_string(&edges).map_err(|e| invalid(e.to_string()))?)
            .bind(map_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE device_outages SET ended_at=unixepoch(),end_reason='Device removed' WHERE map_id=? AND device_key=? AND ended_at IS NULL").bind(map_id).bind(&cell_id).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM topology_nodes WHERE id=? AND version=?")
            .bind(id)
            .bind(version)
            .execute(&mut *tx)
            .await?;
        Self::bump_edit(&mut tx, map_id).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn save_edges(
        &self,
        id: i64,
        version: i64,
        edges: Vec<import::Edge>,
    ) -> Result<(), AppError> {
        if edges.len() > 5000 {
            return Err(invalid("Maximum 5000 connections"));
        }
        let mut tx = self.pool.begin().await?;
        Self::snapshot(&mut tx, id, "connection edit").await?;
        let ids: Vec<String> =
            sqlx::query_scalar("SELECT cell_id FROM topology_nodes WHERE map_id=?")
                .bind(id)
                .fetch_all(&mut *tx)
                .await?;
        let ids: std::collections::HashSet<_> = ids.into_iter().collect();
        for edge in &edges {
            if !ids.contains(&edge.source)
                || !ids.contains(&edge.target)
                || edge.source == edge.target
                || edge.label.chars().count() > 200
                || edge.points.len() > 100
                || edge
                    .points
                    .iter()
                    .flatten()
                    .any(|v| !v.is_finite() || v.abs() > 100000.0)
            {
                return Err(invalid("Invalid connection endpoints or geometry"));
            }
        }
        let data = serde_json::to_string(&edges).map_err(|e| invalid(e.to_string()))?;
        if sqlx::query(
            "UPDATE topology_maps SET edges_json=?,version=version+1 WHERE id=? AND version=?",
        )
        .bind(data)
        .bind(id)
        .bind(version)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 0
        {
            return Err(AppError::Conflict(
                "Map connections changed; refresh first".into(),
            ));
        }
        Self::bump_edit(&mut tx, id).await?;
        tx.commit().await?;
        Ok(())
    }
    pub async fn check_now(&self, map_id: i64) -> Result<(), AppError> {
        sqlx::query("UPDATE topology_nodes SET next_check=0 WHERE map_id=?")
            .bind(map_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
    pub async fn events(&self) -> Result<Vec<NetworkEvent>, AppError> {
        Ok(
            sqlx::query_as("SELECT * FROM network_events ORDER BY id DESC LIMIT 100")
                .fetch_all(&self.pool)
                .await?,
        )
    }
}
