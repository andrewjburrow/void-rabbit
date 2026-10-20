use actix_web::{cookie::Cookie, http::StatusCode, test, web, App};
use serde_json::json;
use void_rabbit::{
    auth::AuthService,
    configure,
    topology::{monitor::Probe, Device, DeviceInput, TopologyService},
    AppState,
};
async fn service() -> TopologyService {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    let service = TopologyService::new(pool);
    service.initialize().await.unwrap();
    service
}
fn input(node: &Device) -> DeviceInput {
    DeviceInput {
        label: node.label.clone(),
        kind: node.kind.clone(),
        ip: node.ip.clone(),
        group_id: node.group_id,
        enabled: node.enabled,
        x: node.x,
        y: node.y,
        width: node.width,
        height: node.height,
        version: Some(node.version),
    }
}
fn new_device() -> DeviceInput {
    DeviceInput {
        label: "Router".into(),
        kind: "router".into(),
        ip: "127.0.0.1".into(),
        group_id: 1,
        enabled: true,
        x: 20.0,
        y: 40.0,
        width: 80.0,
        height: 60.0,
        version: None,
    }
}
#[actix_web::test]
async fn undo_restores_connections_deleted_devices_and_live_health_without_accepting_stale_edits() {
    let service = service().await;
    let id = service
        .import(include_str!("../example-topology.drawio"))
        .await
        .unwrap()[0];
    let map = service.map(id).await.unwrap();
    let node = map.nodes.iter().find(|n| n.kind == "router").unwrap();
    let mut cfg = input(node);
    cfg.ip = "127.0.0.1".into();
    let node = service.save_device(None, Some(node.id), cfg).await.unwrap();
    service
        .apply_probe(node.id, node.version, Probe::Up(0.4))
        .await
        .unwrap();
    let mut cfg = input(&node);
    cfg.label = "Changed router".into();
    cfg.x += 120.0;
    let changed = service.save_device(None, Some(node.id), cfg).await.unwrap();
    let current = service.map(id).await.unwrap();
    service.undo(id, current.edit_revision).await.unwrap();
    let restored = service.map(id).await.unwrap();
    let same = restored.nodes.iter().find(|n| n.id == node.id).unwrap();
    assert_eq!(same.label, node.label);
    assert_eq!(same.x, node.x);
    assert_eq!(same.status, "up");
    assert_eq!(same.latency_ms, Some(0.4));
    assert!(same.version > changed.version);
    assert!(service.undo(id, current.edit_revision).await.is_err());
    service
        .apply_probe(node.id, changed.version, Probe::Down)
        .await
        .unwrap();
    assert_eq!(
        service
            .map(id)
            .await
            .unwrap()
            .nodes
            .iter()
            .find(|n| n.id == node.id)
            .unwrap()
            .status,
        "up"
    );
    let before = service.map(id).await.unwrap();
    let node = before.nodes.iter().find(|n| n.id == node.id).unwrap();
    service.delete_device(node.id, node.version).await.unwrap();
    let after = service.map(id).await.unwrap();
    assert!(after.nodes.len() < before.nodes.len());
    assert!(after.edges.len() < before.edges.len());
    service.undo(id, after.edit_revision).await.unwrap();
    let restored = service.map(id).await.unwrap();
    assert_eq!(restored.nodes.len(), before.nodes.len());
    assert_eq!(
        serde_json::to_value(restored.edges).unwrap(),
        serde_json::to_value(before.edges).unwrap()
    );
    assert_eq!(
        restored
            .nodes
            .iter()
            .find(|n| n.id == node.id)
            .unwrap()
            .status,
        "unknown"
    );
    let map = service.map(id).await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM topology_undo")
        .fetch_one(&service.pool)
        .await
        .unwrap();
    assert!(service
        .save_edges(id, map.version - 1, vec![])
        .await
        .is_err());
    let after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM topology_undo")
        .fetch_one(&service.pool)
        .await
        .unwrap();
    assert_eq!(count, after);
    assert_eq!(
        service.map(id).await.unwrap().edit_revision,
        map.edit_revision
    );
}
#[actix_web::test]
async fn undo_can_walk_multiple_edits_and_history_is_bounded() {
    let service = service().await;
    let id = service.create_map("Undo").await.unwrap();
    let mut node = service
        .save_device(Some(id), None, new_device())
        .await
        .unwrap();
    for n in 0..24 {
        let mut cfg = input(&node);
        cfg.label = format!("Edit {n}");
        node = service.save_device(None, Some(node.id), cfg).await.unwrap();
    }
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM topology_undo WHERE map_id=?")
        .bind(id)
        .fetch_one(&service.pool)
        .await
        .unwrap();
    assert_eq!(count, 20);
    for expected in ["Edit 22", "Edit 21", "Edit 20"] {
        let map = service.map(id).await.unwrap();
        service.undo(id, map.edit_revision).await.unwrap();
        assert_eq!(service.map(id).await.unwrap().nodes[0].label, expected);
    }
}
#[actix_web::test]
async fn outage_history_deduplicates_and_survives_deletion_undo() {
    let service = service().await;
    let id = service.create_map("Health").await.unwrap();
    let node = service
        .save_device(Some(id), None, new_device())
        .await
        .unwrap();
    service
        .apply_probe(node.id, node.version, Probe::Up(0.5))
        .await
        .unwrap();
    let healthy = service.map(id).await.unwrap().nodes.remove(0);
    assert!(healthy.last_success_at > 0);
    service
        .apply_probe(node.id, node.version, Probe::Down)
        .await
        .unwrap();
    service
        .apply_probe(node.id, node.version, Probe::Down)
        .await
        .unwrap();
    let rows = service.outages(node.id).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].ended_at.is_none());
    let failed = service.map(id).await.unwrap().nodes.remove(0);
    assert!(failed.status_since > 0);
    assert_eq!(failed.last_success_at, healthy.last_success_at);
    service
        .apply_probe(node.id, node.version, Probe::Up(0.7))
        .await
        .unwrap();
    let rows = service.outages(node.id).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].end_reason, "Recovered");
    assert!(rows[0].ended_at.unwrap() >= rows[0].started_at);
    service
        .apply_probe(node.id, node.version, Probe::Down)
        .await
        .unwrap();
    service.delete_device(node.id, node.version).await.unwrap();
    let map = service.map(id).await.unwrap();
    service.undo(id, map.edit_revision).await.unwrap();
    let rows = service.outages(node.id).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].end_reason, "Device removed");
    assert!(rows[0].ended_at.is_some());
    service.initialize_history().await.unwrap();
    assert_eq!(service.outages(node.id).await.unwrap().len(), 2);
}
#[actix_web::test]
async fn legacy_database_migrates_without_losing_maps_or_inventory() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    for statement in ["CREATE TABLE assets(id INTEGER PRIMARY KEY,name TEXT)","INSERT INTO assets VALUES (1,'Legacy inventory')","CREATE TABLE topology_maps(id INTEGER PRIMARY KEY AUTOINCREMENT,name TEXT NOT NULL,edges_json TEXT NOT NULL DEFAULT '[]',version INTEGER NOT NULL DEFAULT 1)","INSERT INTO topology_maps(name) VALUES ('Legacy map')","CREATE TABLE topology_nodes(id INTEGER PRIMARY KEY AUTOINCREMENT,map_id INTEGER NOT NULL,cell_id TEXT NOT NULL,label TEXT NOT NULL,kind TEXT NOT NULL,x REAL NOT NULL,y REAL NOT NULL,width REAL NOT NULL,height REAL NOT NULL,ip TEXT NOT NULL DEFAULT '',group_id INTEGER NOT NULL,enabled INTEGER NOT NULL DEFAULT 1,status TEXT NOT NULL DEFAULT 'unknown',failures INTEGER NOT NULL DEFAULT 0,last_checked INTEGER NOT NULL DEFAULT 0,next_check INTEGER NOT NULL DEFAULT 0,latency_ms REAL,probe_error TEXT NOT NULL DEFAULT '',version INTEGER NOT NULL DEFAULT 1,UNIQUE(map_id,cell_id))","INSERT INTO topology_nodes(map_id,cell_id,label,kind,x,y,width,height,ip,group_id,status,last_checked) VALUES(1,'legacy','Legacy router','router',1,2,80,60,'192.0.2.1',1,'down',1700000000)"]{sqlx::query(statement).execute(&pool).await.unwrap();}
    let service = TopologyService::new(pool);
    service.initialize().await.unwrap();
    service.initialize().await.unwrap();
    let map = service.map(1).await.unwrap();
    assert_eq!(map.name, "Legacy map");
    assert_eq!(map.edit_revision, 1);
    assert_eq!(map.nodes[0].status_since, 1700000000);
    assert_eq!(service.outages(map.nodes[0].id).await.unwrap().len(), 1);
    let inventory: String = sqlx::query_scalar("SELECT name FROM assets")
        .fetch_one(&service.pool)
        .await
        .unwrap();
    assert_eq!(inventory, "Legacy inventory");
}
#[actix_web::test]
async fn views_are_private_and_viewers_can_save_their_own_view_but_not_download_backup() {
    let service = service().await;
    let id = service.create_map("Views").await.unwrap();
    let auth = AuthService::initialize(service.pool.clone(), false)
        .await
        .unwrap();
    auth.bootstrap(&auth.setup_key, "admin", "admin-password-123".into())
        .await
        .unwrap();
    auth.add_user("viewer", "viewer-password-123".into(), "viewer")
        .await
        .unwrap();
    void_rabbit::preferences::initialize(&service.pool)
        .await
        .unwrap();
    let (token, session) = auth
        .new_session(
            auth.login("viewer", "viewer-password-123".into())
                .await
                .unwrap(),
        )
        .await
        .unwrap();
    let view_cookie = Cookie::new("topology_session", token);
    let (admin_token, _) = auth
        .new_session(
            auth.login("admin", "admin-password-123".into())
                .await
                .unwrap(),
        )
        .await
        .unwrap();
    let state = web::Data::new(AppState {
        pool: service.pool.clone(),
        auth,
    });
    let app = test::init_service(App::new().app_data(state).configure(configure)).await;
    let value = json!({"viewport":{"x":25.0,"y":90.0,"w":700.0,"h":500.0},"height":800.0,"focus":true,"device_list":true,"noc":false});
    let response = test::call_service(
        &app,
        test::TestRequest::put()
            .uri(&format!("/api/preferences/views/{id}"))
            .cookie(view_cookie.clone())
            .insert_header(("X-CSRF-Token", session.csrf_token.clone()))
            .set_json(&value)
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let response = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/api/preferences/views/{id}"))
            .cookie(view_cookie.clone())
            .to_request(),
    )
    .await;
    let saved: serde_json::Value = test::read_body_json(response).await;
    assert_eq!(saved, value);
    let response = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/api/preferences/views/{id}"))
            .cookie(Cookie::new("topology_session", admin_token))
            .to_request(),
    )
    .await;
    let saved: serde_json::Value = test::read_body_json(response).await;
    assert!(saved.is_null());
    let response = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/api/backup")
            .cookie(view_cookie.clone())
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let mut invalid = value;
    invalid["viewport"]["w"] = json!(0);
    let response = test::call_service(
        &app,
        test::TestRequest::put()
            .uri(&format!("/api/preferences/views/{id}"))
            .cookie(view_cookie)
            .insert_header(("X-CSRF-Token", session.csrf_token))
            .set_json(invalid)
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}
#[actix_web::test]
async fn backup_is_restorable_and_recovery_revokes_admin_sessions() {
    let live_path = std::env::temp_dir().join(format!(
        "rabbit-live-test-{}.sqlite",
        void_rabbit::auth::random_secret()
    ));
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(&live_path)
                .create_if_missing(true)
                .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal),
        )
        .await
        .unwrap();
    let service = TopologyService::new(pool);
    service.initialize().await.unwrap();
    service.create_map("Backed up map").await.unwrap();
    let auth = AuthService::initialize(service.pool.clone(), false)
        .await
        .unwrap();
    let admin = auth
        .bootstrap(&auth.setup_key, "admin", "old-password-123".into())
        .await
        .unwrap();
    let (token, _) = auth.new_session(admin).await.unwrap();
    auth.add_user("viewer", "viewer-password-123".into(), "viewer")
        .await
        .unwrap();
    void_rabbit::preferences::initialize(&service.pool)
        .await
        .unwrap();
    let state = web::Data::new(AppState {
        pool: service.pool.clone(),
        auth: auth.clone(),
    });
    let app = test::init_service(App::new().app_data(state).configure(configure)).await;
    let response = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/api/backup")
            .cookie(Cookie::new("topology_session", token.clone()))
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = test::read_body(response).await;
    assert!(bytes.starts_with(b"SQLite format 3\0"));
    let path = std::env::temp_dir().join(format!(
        "rabbit-test-backup-{}.sqlite",
        void_rabbit::auth::random_secret()
    ));
    std::fs::write(&path, &bytes).unwrap();
    let backup = sqlx::sqlite::SqlitePoolOptions::new()
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(&path)
                .read_only(true),
        )
        .await
        .unwrap();
    let integrity: String = sqlx::query_scalar("PRAGMA integrity_check")
        .fetch_one(&backup)
        .await
        .unwrap();
    assert_eq!(integrity, "ok");
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(&backup)
        .await
        .unwrap();
    assert_eq!(count, 2);
    let name: String = sqlx::query_scalar("SELECT name FROM topology_maps")
        .fetch_one(&backup)
        .await
        .unwrap();
    assert_eq!(name, "Backed up map");
    backup.close().await;
    std::fs::remove_file(path).unwrap();
    assert!(
        void_rabbit::backup::recover_admin(&auth, "viewer", "new-password-123".into())
            .await
            .is_err()
    );
    void_rabbit::backup::recover_admin(&auth, "admin", "new-password-123".into())
        .await
        .unwrap();
    assert!(auth
        .login("admin", "old-password-123".into())
        .await
        .is_err());
    assert!(auth.login("admin", "new-password-123".into()).await.is_ok());
    let response = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/api/auth/me")
            .cookie(Cookie::new("topology_session", token))
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    service.pool.close().await;
    std::fs::remove_file(live_path).unwrap();
}
