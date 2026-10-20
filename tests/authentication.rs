use actix_web::{cookie::Cookie, http::StatusCode, test, web, App};
use serde_json::json;
use void_rabbit::{
    auth::AuthService,
    configure,
    topology::{
        monitor::{parse_output, ping_arguments, Probe},
        TopologyService,
    },
    AppState,
};
async fn state() -> web::Data<AppState> {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    TopologyService::new(pool.clone())
        .initialize()
        .await
        .unwrap();
    let auth = AuthService::initialize(pool.clone(), false).await.unwrap();
    void_rabbit::preferences::initialize(&pool).await.unwrap();
    web::Data::new(AppState { pool, auth })
}
#[test]
async fn platform_ping_flags_and_actual_echo_detection() {
    let ip = "192.0.2.1".parse().unwrap();
    assert_eq!(
        ping_arguments(ip, true),
        ["-4", "-n", "1", "-w", "2000", "192.0.2.1"]
    );
    assert_eq!(
        ping_arguments(ip, false),
        ["-4", "-n", "-c", "1", "-W", "2", "192.0.2.1"]
    );
    assert_eq!(ping_arguments("::1".parse().unwrap(), true)[0], "-6");
    assert!(
        matches!(parse_output(true,Some(0),"Reply from 192.0.2.1: bytes=32 time<1ms TTL=64","",true,2.0),Probe::Up(v) if v==0.5)
    );
    assert!(
        matches!(parse_output(true,Some(0),"Reply from ::1: time=12ms","",true,20.0),Probe::Up(v) if v==12.0)
    );
    assert!(matches!(
        parse_output(
            true,
            Some(0),
            "Reply from 192.0.2.254: Destination host unreachable.",
            "",
            true,
            2.0
        ),
        Probe::Down
    ));
    assert!(
        matches!(parse_output(true,Some(0),"Ответ от ::1: время<1мс","",true,20.0),Probe::Up(v) if v==0.5)
    );
    assert!(matches!(
        parse_output(
            false,
            Some(1),
            "PING: transmit failed. General failure.",
            "",
            true,
            2.0
        ),
        Probe::Error(_)
    ));
    assert!(matches!(
        parse_output(false, Some(1), "Request timed out.", "", true, 2.0),
        Probe::Down
    ));
    assert!(matches!(
        parse_output(false, Some(2), "", "invalid option", false, 2.0),
        Probe::Error(_)
    ));
    assert!(
        matches!(parse_output(true,Some(0),"64 bytes from 127.0.0.1: icmp_seq=1 ttl=64 time=0.042 ms","",false,2.0),Probe::Up(v) if v==0.042)
    );
}
#[actix_web::test]
async fn setup_login_csrf_roles_session_revocation_and_logging() {
    let state = state().await;
    let key = state.auth.setup_key.to_string();
    let app = test::init_service(App::new().app_data(state.clone()).configure(configure)).await;
    let response = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/api/auth/setup")
            .set_json(
                json!({"setup_key":"bad","username":"admin","password":"strong-password-123"}),
            )
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let response = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/api/auth/setup")
            .set_json(json!({"setup_key":key,"username":"admin","password":"strong-password-123"}))
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let cookie = response.response().cookies().next().unwrap().into_owned();
    assert_eq!(cookie.http_only(), Some(true));
    assert_eq!(
        cookie.same_site(),
        Some(actix_web::cookie::SameSite::Strict)
    );
    let session: serde_json::Value = test::read_body_json(response).await;
    let csrf = session["csrf_token"].as_str().unwrap();
    let hash: String = sqlx::query_scalar("SELECT password_hash FROM users")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert!(hash.starts_with("$argon2id$"));
    assert!(!hash.contains("strong-password"));
    let stored: String = sqlx::query_scalar("SELECT token_hash FROM sessions")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_ne!(stored, cookie.value());
    let response = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/api/auth/setup")
            .set_json(
                json!({"setup_key":key,"username":"another","password":"strong-password-123"}),
            )
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let response = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/api/topology/maps")
            .cookie(cookie.clone())
            .set_json(json!({"name":"Lab"}))
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/api/topology/maps")
            .cookie(cookie.clone())
            .insert_header(("X-CSRF-Token", csrf))
            .set_json(json!({"name":"Lab"}))
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let viewer = state
        .auth
        .add_user("viewer", "viewer-password-123".into(), "viewer")
        .await
        .unwrap();
    let (token, view_session) = state
        .auth
        .new_session(
            state
                .auth
                .login("viewer", "viewer-password-123".into())
                .await
                .unwrap(),
        )
        .await
        .unwrap();
    let view_cookie = Cookie::new("topology_session", token);
    let response = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/api/topology/maps")
            .cookie(view_cookie.clone())
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    for path in ["/api/topology/smtp", "/api/auth/users"] {
        let response = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(path)
                .cookie(view_cookie.clone())
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
    let response = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/api/topology/maps")
            .cookie(view_cookie.clone())
            .insert_header(("X-CSRF-Token", view_session.csrf_token.clone()))
            .set_json(json!({"name":"Illegal"}))
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = test::call_service(
        &app,
        test::TestRequest::put()
            .uri("/api/preferences")
            .cookie(view_cookie.clone())
            .insert_header(("X-CSRF-Token", view_session.csrf_token.clone()))
            .set_json(
                json!({"theme":"light","accent":"#508fd0","sound_enabled":true,"sound_name":""}),
            )
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(state
        .auth
        .update_user(1, "viewer", true, None)
        .await
        .is_err());
    state
        .auth
        .update_user(viewer.id, "viewer", false, None)
        .await
        .unwrap();
    let response = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/api/topology/maps")
            .cookie(view_cookie)
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let response = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/api/activity")
            .cookie(cookie.clone())
            .to_request(),
    )
    .await;
    let events: serde_json::Value = test::read_body_json(response).await;
    assert!(events
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["action"] == "Map created" && e["actor"] == "admin"));
    assert!(events
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["detail"].as_str().unwrap().contains("Failed attempt")));
    assert!(!events.to_string().contains("strong-password"));
    let response=test::call_service(&app,test::TestRequest::post().uri("/api/auth/password").cookie(cookie.clone()).insert_header(("X-CSRF-Token",csrf)).set_json(json!({"current_password":"strong-password-123","new_password":"new-password-12345"})).to_request()).await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let response = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/api/auth/me")
            .cookie(cookie)
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(state
        .auth
        .login("admin", "strong-password-123".into())
        .await
        .is_err());
    assert!(state
        .auth
        .login("admin", "new-password-12345".into())
        .await
        .is_ok());
    for _ in 0..10 {
        state.auth.rate_limit("test-ip").unwrap();
    }
    assert!(state.auth.rate_limit("test-ip").is_err());
}
#[actix_web::test]
async fn custom_audio_is_private_validated_and_survives_preferences_save() {
    let state = state().await;
    let user = state
        .auth
        .bootstrap(&state.auth.setup_key, "admin", "strong-password-123".into())
        .await
        .unwrap();
    let (token, session) = state.auth.new_session(user).await.unwrap();
    let cookie = Cookie::new("topology_session", token);
    let app = test::init_service(App::new().app_data(state).configure(configure)).await;
    let response = test::call_service(
        &app,
        test::TestRequest::put()
            .uri("/api/preferences/sound")
            .cookie(cookie.clone())
            .insert_header(("X-CSRF-Token", session.csrf_token.clone()))
            .set_payload("<script>alert(1)</script>")
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let audio = b"RIFF\0\0\0\0WAVEfmt ";
    let response = test::call_service(
        &app,
        test::TestRequest::put()
            .uri("/api/preferences/sound")
            .cookie(cookie.clone())
            .insert_header(("X-CSRF-Token", session.csrf_token.clone()))
            .insert_header(("X-Filename", "alarm.wav"))
            .set_payload(audio.as_slice())
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let response = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/api/preferences/sound")
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let response = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/api/preferences/sound")
            .cookie(cookie.clone())
            .to_request(),
    )
    .await;
    assert_eq!(response.headers().get("content-type").unwrap(), "audio/wav");
    assert_eq!(test::read_body(response).await, audio.as_slice());
    let response = test::call_service(
        &app,
        test::TestRequest::put()
            .uri("/api/preferences")
            .cookie(cookie.clone())
            .insert_header(("X-CSRF-Token", session.csrf_token.clone()))
            .set_json(
                json!({"theme":"dark","accent":"#478061","sound_enabled":true,"sound_name":""}),
            )
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let response = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/api/preferences")
            .cookie(cookie.clone())
            .to_request(),
    )
    .await;
    let prefs: serde_json::Value = test::read_body_json(response).await;
    assert_eq!(prefs["sound_name"], "alarm.wav");
    let response = test::call_service(
        &app,
        test::TestRequest::delete()
            .uri("/api/preferences/sound")
            .cookie(cookie)
            .insert_header(("X-CSRF-Token", session.csrf_token))
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}
