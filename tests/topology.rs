use base64::{engine::general_purpose::STANDARD, Engine};
use flate2::{write::DeflateEncoder, Compression};
use sqlx::sqlite::SqlitePoolOptions;
use std::io::Write;
use void_rabbit::{
    error::AppError,
    topology::{
        import,
        monitor::Probe,
        smtp::{Config, ConfigInput},
        DeviceInput, GroupInput, TopologyService,
    },
};

async fn service() -> TopologyService {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    let service = TopologyService::new(pool);
    service.initialize().await.unwrap();
    service
}
fn device(ip: &str) -> DeviceInput {
    DeviceInput {
        label: "Test router".into(),
        kind: "router".into(),
        ip: ip.into(),
        group_id: 1,
        enabled: true,
        x: 0.0,
        y: 0.0,
        width: 80.0,
        height: 60.0,
        version: None,
    }
}

#[test]
fn drawio_compression_nested_layout_and_types() {
    let xml = include_str!("../example-topology.drawio");
    let pages = import::pages(xml).unwrap();
    let graph = &pages[0].1;
    let diagram = import::parse("Example", graph).unwrap();
    assert_eq!(diagram.nodes.len(), 6);
    assert_eq!(diagram.edges.len(), 4);
    let router = diagram.nodes.iter().find(|n| n.cell_id == "edge").unwrap();
    assert_eq!(router.kind, "router");
    assert_eq!(
        diagram
            .nodes
            .iter()
            .find(|n| n.cell_id == "switch")
            .unwrap()
            .kind,
        "switch"
    );
    assert_eq!(router.x, 340.0);
    assert_eq!(router.y, 245.0);
    let encoded = percent_encoding::utf8_percent_encode(graph, percent_encoding::NON_ALPHANUMERIC)
        .to_string();
    let mut compressor = DeflateEncoder::new(Vec::new(), Compression::default());
    compressor.write_all(encoded.as_bytes()).unwrap();
    let encoded = STANDARD.encode(compressor.finish().unwrap());
    let compressed = format!("<mxfile><diagram name='Compressed'>{encoded}</diagram></mxfile>");
    let pages = import::pages(&compressed).unwrap();
    assert_eq!(
        import::parse(&pages[0].0, &pages[0].1).unwrap().nodes.len(),
        6
    );
    assert!(import::pages("<html/>").is_err());
    assert!(import::pages("<mxfile><diagram>BAD BASE64</diagram></mxfile>").is_err());
    let cycle="<mxGraphModel><root><mxCell id='a' parent='b' vertex='1'><mxGeometry/></mxCell><mxCell id='b' parent='a' vertex='1'><mxGeometry/></mxCell></root></mxGraphModel>";
    assert!(import::parse("Cycle", cycle).is_err());
    let metadata="<mxGraphModel><root><UserObject id='wrapped' label='Core router' ip='192.0.2.1'><mxCell vertex='1' parent='1'><mxGeometry x='4' y='5'/></mxCell></UserObject></root></mxGraphModel>";
    let diagram = import::parse("metadata", metadata).unwrap();
    assert_eq!(diagram.nodes[0].cell_id, "wrapped");
    assert_eq!(diagram.nodes[0].ip, "192.0.2.1");
}

#[actix_web::test]
async fn maps_groups_device_validation_and_connection_revisions() {
    let service = service().await;
    let maps = service
        .import(include_str!("../example-topology.drawio"))
        .await
        .unwrap();
    let map = service.map(maps[0]).await.unwrap();
    assert_eq!(map.edges.len(), 4);
    assert_eq!(service.groups().await.unwrap()[0].interval_min, 1);
    assert!(matches!(
        service
            .save_device(Some(map.id), None, device("bad-ip"))
            .await,
        Err(AppError::Validation(_))
    ));
    assert!(service
        .save_device(Some(map.id), None, device("0.0.0.0"))
        .await
        .is_err());
    let node = service
        .save_device(Some(map.id), None, device("127.0.0.1"))
        .await
        .unwrap();
    let mut update = device("::1");
    update.version = Some(node.version);
    let node = service
        .save_device(None, Some(node.id), update)
        .await
        .unwrap();
    assert_eq!(node.version, 2);
    let mut stale = device("127.0.0.1");
    stale.version = Some(1);
    assert!(matches!(
        service.save_device(None, Some(node.id), stale).await,
        Err(AppError::Conflict(_))
    ));
    assert!(service
        .save_group(
            None,
            GroupInput {
                name: "Wrong".into(),
                interval_min: 11,
                failure_threshold: 1,
                enabled: true,
                alerts: false,
                version: None
            }
        )
        .await
        .is_err());
    service
        .save_edges(map.id, map.version, map.edges.clone())
        .await
        .unwrap();
    assert!(service
        .save_edges(map.id, map.version, map.edges.clone())
        .await
        .is_err());
    let affected = map.nodes.iter().find(|n| n.cell_id == "edge").unwrap();
    service
        .delete_device(affected.id, affected.version)
        .await
        .unwrap();
    let updated = service.map(map.id).await.unwrap();
    assert_eq!(updated.edges.len(), 2);
    service.delete_map(map.id).await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM topology_nodes")
        .fetch_one(&service.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[actix_web::test]
async fn monitoring_thresholds_stale_probes_pause_and_transition_alerts() {
    let service = service().await;
    let id = service.create_map("Monitoring").await.unwrap();
    let group = service
        .save_group(
            Some(1),
            GroupInput {
                name: "Default".into(),
                interval_min: 10,
                failure_threshold: 2,
                enabled: true,
                alerts: true,
                version: Some(1),
            },
        )
        .await
        .unwrap();
    service
        .save_smtp(ConfigInput {
            config: Config {
                enabled: true,
                host: "localhost".into(),
                port: 25,
                security: "none".into(),
                from: "itam@example.com".into(),
                recipients: "noc@example.com".into(),
                ..Default::default()
            },
            clear_password: false,
        })
        .await
        .unwrap();
    let node = service
        .save_device(Some(id), None, device("127.0.0.1"))
        .await
        .unwrap();
    assert_eq!(service.due().await.unwrap().len(), 1);
    service
        .apply_probe(node.id, node.version, Probe::Down)
        .await
        .unwrap();
    assert_eq!(service.map(id).await.unwrap().nodes[0].status, "unknown");
    assert!(service.due().await.unwrap().is_empty());
    service
        .apply_probe(node.id, node.version, Probe::Down)
        .await
        .unwrap();
    assert_eq!(service.map(id).await.unwrap().nodes[0].status, "down");
    assert_eq!(service.deliveries().await.unwrap().len(), 1);
    service
        .apply_probe(node.id, node.version, Probe::Down)
        .await
        .unwrap();
    assert_eq!(service.deliveries().await.unwrap().len(), 1);
    service
        .apply_probe(node.id, node.version, Probe::Up(1.25))
        .await
        .unwrap();
    assert_eq!(service.deliveries().await.unwrap().len(), 2);
    assert_eq!(service.events().await.unwrap().len(), 2);
    service
        .apply_probe(
            node.id,
            node.version,
            Probe::Error("No ping installed".into()),
        )
        .await
        .unwrap();
    let n = &service.map(id).await.unwrap().nodes[0];
    assert_eq!(n.status, "up");
    assert!(!n.probe_error.is_empty());
    let mut input = device("192.0.2.4");
    input.version = Some(node.version);
    let changed = service
        .save_device(None, Some(node.id), input)
        .await
        .unwrap();
    service
        .apply_probe(node.id, node.version, Probe::Up(0.5))
        .await
        .unwrap();
    assert_eq!(service.map(id).await.unwrap().nodes[0].status, "unknown");
    service
        .save_group(
            Some(1),
            GroupInput {
                name: "Default".into(),
                interval_min: 1,
                failure_threshold: 1,
                enabled: false,
                alerts: true,
                version: Some(group.version),
            },
        )
        .await
        .unwrap();
    service.check_now(id).await.unwrap();
    assert!(service.due().await.unwrap().is_empty());
    service
        .apply_probe(changed.id, changed.version, Probe::Down)
        .await
        .unwrap();
    assert_eq!(service.map(id).await.unwrap().nodes[0].status, "unknown");
}

#[actix_web::test]
async fn smtp_secret_redaction_preservation_and_revision() {
    let service = service().await;
    let config = Config {
        username: "user".into(),
        password: "secret-password".into(),
        ..Default::default()
    };
    let stored = service
        .save_smtp(ConfigInput {
            config,
            clear_password: false,
        })
        .await
        .unwrap();
    assert_eq!(stored.public()["password_set"], true);
    assert!(stored.public().get("password").is_none());
    let mut input = stored.clone();
    input.password.clear();
    let saved = service
        .save_smtp(ConfigInput {
            config: input.clone(),
            clear_password: false,
        })
        .await
        .unwrap();
    assert_eq!(saved.password, "secret-password");
    assert!(matches!(
        service
            .save_smtp(ConfigInput {
                config: input,
                clear_password: false
            })
            .await,
        Err(AppError::Conflict(_))
    ));
    let mut clear = saved;
    clear.password.clear();
    let cleared = service
        .save_smtp(ConfigInput {
            config: clear,
            clear_password: true,
        })
        .await
        .unwrap();
    assert!(cleared.password.is_empty());
}

#[actix_web::test]
async fn smtp_delivery_to_local_test_relay() {
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::TcpListener,
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let relay = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let (read, mut write) = socket.into_split();
        let mut lines = BufReader::new(read).lines();
        write
            .write_all(b"220 localhost test SMTP\r\n")
            .await
            .unwrap();
        let mut in_data = false;
        let mut message = String::new();
        while let Some(line) = lines.next_line().await.unwrap() {
            let reply = if in_data {
                if line == "." {
                    in_data = false;
                    b"250 Accepted\r\n".as_slice()
                } else {
                    message.push_str(&line);
                    message.push('\n');
                    continue;
                }
            } else if line.starts_with("EHLO") || line.starts_with("HELO") {
                b"250-localhost\r\n250 8BITMIME\r\n".as_slice()
            } else if line.starts_with("DATA") {
                in_data = true;
                b"354 Send message\r\n".as_slice()
            } else if line.starts_with("QUIT") {
                write.write_all(b"221 Bye\r\n").await.unwrap();
                break;
            } else {
                b"250 OK\r\n".as_slice()
            };
            write.write_all(reply).await.unwrap();
        }
        message
    });
    let service = service().await;
    service
        .save_smtp(ConfigInput {
            config: Config {
                enabled: true,
                host: "127.0.0.1".into(),
                port,
                security: "none".into(),
                from: "itam@example.com".into(),
                recipients: "noc@example.com".into(),
                ..Default::default()
            },
            clear_password: false,
        })
        .await
        .unwrap();
    service.queue_test().await.unwrap();
    assert!(service.deliver_one().await.unwrap());
    let rows = service.deliveries().await.unwrap();
    assert_eq!(rows[0]["state"], "sent");
    let message = tokio::time::timeout(std::time::Duration::from_secs(3), relay)
        .await
        .unwrap()
        .unwrap();
    assert!(message.contains("Your Void Rabbit SMTP"));
    assert!(message.contains("noc@example.com"));
}

#[actix_web::test]
async fn topology_routes_share_auth_and_accept_diagram_uploads_over_inventory_json_limit() {
    use actix_web::{http::StatusCode, test, web, App};
    use void_rabbit::{auth::AuthService, configure, AppState};
    let service = service().await;
    let auth = AuthService::initialize(service.pool.clone(), false)
        .await
        .unwrap();
    let user = auth
        .bootstrap(&auth.setup_key, "admin", "testing-password-123".into())
        .await
        .unwrap();
    let (token, session) = auth.new_session(user).await.unwrap();
    let state = web::Data::new(AppState {
        pool: service.pool.clone(),
        auth,
    });
    let app = test::init_service(App::new().app_data(state).configure(configure)).await;
    for uri in [
        "/api/topology/maps",
        "/api/topology/groups",
        "/api/topology/events",
        "/api/topology/smtp",
        "/api/topology/deliveries",
    ] {
        let response =
            test::call_service(&app, test::TestRequest::get().uri(uri).to_request()).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    let response = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/api/topology/import")
            .set_json(serde_json::json!({"xml":include_str!("../example-topology.drawio")}))
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let xml = include_str!("../example-topology.drawio").replace(
        "<mxfile ",
        &format!("<!--{}--><mxfile ", "x".repeat(100_000)),
    );
    let response = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/api/topology/import")
            .cookie(actix_web::cookie::Cookie::new(
                "topology_session",
                token.clone(),
            ))
            .insert_header(("X-CSRF-Token", session.csrf_token.clone()))
            .set_json(serde_json::json!({"xml":xml}))
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let response = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/api/topology/smtp")
            .cookie(actix_web::cookie::Cookie::new(
                "topology_session",
                token.clone(),
            ))
            .insert_header(("X-CSRF-Token", session.csrf_token.clone()))
            .to_request(),
    )
    .await;
    let config: serde_json::Value = test::read_body_json(response).await;
    assert!(config.get("password").is_none());
    assert_eq!(config["password_set"], false);
}
