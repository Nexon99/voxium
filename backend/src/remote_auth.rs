use actix_web::{web, HttpResponse};
use base64::{engine::general_purpose, Engine};
use futures_util::{SinkExt, StreamExt};
use rsa::{pkcs8::EncodePublicKey, rand_core::OsRng, Oaep, RsaPrivateKey, RsaPublicKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::Message;

const DISCORD_REMOTE_AUTH_GATEWAY: &str = "wss://remote-auth-gateway.discord.gg/?v=2";
const DISCORD_REMOTE_AUTH_LOGIN_API: &str =
    "https://discord.com/api/v9/users/@me/remote-auth/login";
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

// ── Session types ───────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status")]
pub enum QrStatus {
    #[serde(rename = "connecting")]
    Connecting,
    #[serde(rename = "waiting_for_qr")]
    WaitingForQr,
    #[serde(rename = "qr_ready")]
    QrReady { qr_url: String, ra_url: String },
    #[serde(rename = "scanned")]
    Scanned,
    #[serde(rename = "completing")]
    Completing,
    #[serde(rename = "completed")]
    Completed { auth: serde_json::Value },
    #[serde(rename = "error")]
    Error { message: String },
    #[serde(rename = "cancelled")]
    Cancelled,
}

pub(crate) struct QrSession {
    status: QrStatus,
    cancel_tx: Option<mpsc::Sender<()>>,
}

pub type QrAuthSessions = Arc<Mutex<HashMap<String, QrSession>>>;

pub fn create_qr_sessions() -> QrAuthSessions {
    Arc::new(Mutex::new(HashMap::new()))
}

// ── Request types ───────────────────────────────────────

#[derive(Deserialize)]
pub struct SessionQuery {
    pub session_id: String,
}

#[derive(Deserialize)]
pub struct CancelPayload {
    pub session_id: String,
}

// ── Handlers ────────────────────────────────────────────

pub async fn start_qr_session(
    pool: web::Data<SqlitePool>,
    sessions: web::Data<QrAuthSessions>,
) -> HttpResponse {
    let session_id = uuid::Uuid::new_v4().to_string();
    let (cancel_tx, cancel_rx) = mpsc::channel(1);

    // Clean finished sessions
    {
        let mut map = sessions.lock().await;
        map.retain(|_, s| {
            matches!(
                s.status,
                QrStatus::Connecting
                    | QrStatus::WaitingForQr
                    | QrStatus::QrReady { .. }
                    | QrStatus::Scanned
                    | QrStatus::Completing
            )
        });
        map.insert(
            session_id.clone(),
            QrSession {
                status: QrStatus::Connecting,
                cancel_tx: Some(cancel_tx),
            },
        );
    }

    let sessions_clone = sessions.get_ref().clone();
    let pool_clone = pool.get_ref().clone();
    let sid = session_id.clone();
    tokio::spawn(async move {
        run_remote_auth_flow(sid, sessions_clone, pool_clone, cancel_rx).await;
    });

    HttpResponse::Ok().json(serde_json::json!({ "session_id": session_id }))
}

pub async fn get_qr_status(
    sessions: web::Data<QrAuthSessions>,
    query: web::Query<SessionQuery>,
) -> HttpResponse {
    let map = sessions.lock().await;
    if let Some(session) = map.get(&query.session_id) {
        HttpResponse::Ok().json(&session.status)
    } else {
        HttpResponse::NotFound().json(serde_json::json!({ "error": "Session introuvable" }))
    }
}

pub async fn cancel_qr_session(
    sessions: web::Data<QrAuthSessions>,
    body: web::Json<CancelPayload>,
) -> HttpResponse {
    let mut map = sessions.lock().await;
    if let Some(session) = map.get_mut(&body.session_id) {
        if let Some(tx) = session.cancel_tx.take() {
            let _ = tx.try_send(());
        }
        session.status = QrStatus::Cancelled;
        HttpResponse::Ok().json(serde_json::json!({ "ok": true }))
    } else {
        HttpResponse::NotFound().json(serde_json::json!({ "error": "Session introuvable" }))
    }
}

// ── Internal helpers ────────────────────────────────────

async fn set_status(sessions: &QrAuthSessions, session_id: &str, status: QrStatus) {
    let mut map = sessions.lock().await;
    if let Some(session) = map.get_mut(session_id) {
        session.status = status;
    }
}

fn generate_qr_data_uri(data: &str) -> Result<String, String> {
    use image::ImageEncoder;
    use qrcode::QrCode;

    let code = QrCode::new(data.as_bytes()).map_err(|e| format!("QR encode error: {e}"))?;
    let img = code.render::<image::Luma<u8>>().quiet_zone(true).min_dimensions(256, 256).build();
    let mut png_buf: Vec<u8> = Vec::new();
    let encoder = image::codecs::png::PngEncoder::new(&mut png_buf);
    encoder
        .write_image(img.as_raw(), img.width(), img.height(), image::ExtendedColorType::L8)
        .map_err(|e| format!("PNG encode error: {e}"))?;
    Ok(format!("data:image/png;base64,{}", general_purpose::STANDARD.encode(&png_buf)))
}

// ── Main flow ───────────────────────────────────────────

async fn run_remote_auth_flow(
    session_id: String,
    sessions: QrAuthSessions,
    pool: SqlitePool,
    mut cancel_rx: mpsc::Receiver<()>,
) {
    // Generate RSA-OAEP 2048 key pair
    let private_key = match RsaPrivateKey::new(&mut OsRng, 2048) {
        Ok(k) => k,
        Err(e) => {
            set_status(
                &sessions,
                &session_id,
                QrStatus::Error {
                    message: format!("RSA keygen error: {e}"),
                },
            )
            .await;
            return;
        }
    };
    let public_key = RsaPublicKey::from(&private_key);
    let encoded_public_key = match public_key.to_public_key_der() {
        Ok(der) => general_purpose::STANDARD.encode(der.as_ref()),
        Err(e) => {
            set_status(
                &sessions,
                &session_id,
                QrStatus::Error {
                    message: format!("SPKI export error: {e}"),
                },
            )
            .await;
            return;
        }
    };

    // Connect to Discord Remote Auth Gateway with proper Origin
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::http::HeaderValue;

    let mut request = match DISCORD_REMOTE_AUTH_GATEWAY.into_client_request() {
        Ok(r) => r,
        Err(e) => {
            set_status(
                &sessions,
                &session_id,
                QrStatus::Error {
                    message: format!("Request build error: {e}"),
                },
            )
            .await;
            return;
        }
    };
    request
        .headers_mut()
        .insert("Origin", HeaderValue::from_static("https://discord.com"));
    request
        .headers_mut()
        .insert("User-Agent", HeaderValue::from_static(USER_AGENT));

    let ws_stream = match tokio_tungstenite::connect_async(request).await {
        Ok((stream, _)) => stream,
        Err(e) => {
            set_status(
                &sessions,
                &session_id,
                QrStatus::Error {
                    message: format!("WebSocket connection failed: {e}"),
                },
            )
            .await;
            return;
        }
    };

    set_status(&sessions, &session_id, QrStatus::WaitingForQr).await;
    let (write, mut read) = ws_stream.split();
    let write = Arc::new(Mutex::new(write));
    let mut heartbeat_handle: Option<tokio::task::JoinHandle<()>> = None;

    loop {
        tokio::select! {
            _ = cancel_rx.recv() => {
                set_status(&sessions, &session_id, QrStatus::Cancelled).await;
                break;
            }
            msg = read.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        let payload: serde_json::Value = match serde_json::from_str(&text) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        let op = payload.get("op").and_then(|v| v.as_str()).unwrap_or("");

                        match op {
                            "hello" => {
                                let interval_ms = payload
                                    .get("heartbeat_interval")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(41250);

                                // Start heartbeat
                                if let Some(h) = heartbeat_handle.take() {
                                    h.abort();
                                }
                                let w = write.clone();
                                heartbeat_handle = Some(tokio::spawn(async move {
                                    let mut interval = tokio::time::interval(
                                        tokio::time::Duration::from_millis(interval_ms),
                                    );
                                    loop {
                                        interval.tick().await;
                                        let msg =
                                            serde_json::json!({"op":"heartbeat"}).to_string();
                                        let mut guard = w.lock().await;
                                        if guard
                                            .send(Message::Text(msg))
                                            .await
                                            .is_err()
                                        {
                                            break;
                                        }
                                    }
                                }));

                                // Send init with public key
                                let init = serde_json::json!({
                                    "op": "init",
                                    "encoded_public_key": encoded_public_key,
                                })
                                .to_string();
                                let mut guard = write.lock().await;
                                if guard.send(Message::Text(init)).await.is_err() {
                                    set_status(
                                        &sessions,
                                        &session_id,
                                        QrStatus::Error {
                                            message: "Failed to send init".into(),
                                        },
                                    )
                                    .await;
                                    break;
                                }
                            }
                            "nonce_proof" => {
                                let enc_nonce = payload
                                    .get("encrypted_nonce")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");

                                let encrypted =
                                    match general_purpose::STANDARD.decode(enc_nonce) {
                                        Ok(b) => b,
                                        Err(_) => {
                                            set_status(
                                                &sessions,
                                                &session_id,
                                                QrStatus::Error {
                                                    message: "Bad nonce base64".into(),
                                                },
                                            )
                                            .await;
                                            break;
                                        }
                                    };

                                let padding = Oaep::new::<Sha256>();
                                let decrypted =
                                    match private_key.decrypt(padding, &encrypted) {
                                        Ok(d) => d,
                                        Err(e) => {
                                            set_status(
                                                &sessions,
                                                &session_id,
                                                QrStatus::Error {
                                                    message: format!(
                                                        "Nonce decrypt error: {e}"
                                                    ),
                                                },
                                            )
                                            .await;
                                            break;
                                        }
                                    };

                                // SHA-256 the decrypted nonce, URL-safe base64 no padding
                                let hash = Sha256::digest(&decrypted);
                                let proof =
                                    general_purpose::URL_SAFE_NO_PAD.encode(hash);

                                let proof_msg = serde_json::json!({
                                    "op": "nonce_proof",
                                    "proof": proof,
                                })
                                .to_string();
                                let mut guard = write.lock().await;
                                if guard
                                    .send(Message::Text(proof_msg))
                                    .await
                                    .is_err()
                                {
                                    set_status(
                                        &sessions,
                                        &session_id,
                                        QrStatus::Error {
                                            message: "Failed to send nonce_proof".into(),
                                        },
                                    )
                                    .await;
                                    break;
                                }
                            }
                            "pending_remote_init" => {
                                let fingerprint = payload
                                    .get("fingerprint")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");

                                if fingerprint.is_empty() {
                                    set_status(
                                        &sessions,
                                        &session_id,
                                        QrStatus::Error {
                                            message: "Empty fingerprint".into(),
                                        },
                                    )
                                    .await;
                                    break;
                                }

                                let ra_url =
                                    format!("https://discord.com/ra/{fingerprint}");
                                let qr_url = match generate_qr_data_uri(&ra_url) {
                                    Ok(uri) => uri,
                                    Err(e) => {
                                        set_status(
                                            &sessions,
                                            &session_id,
                                            QrStatus::Error { message: e },
                                        )
                                        .await;
                                        break;
                                    }
                                };

                                set_status(
                                    &sessions,
                                    &session_id,
                                    QrStatus::QrReady { qr_url, ra_url },
                                )
                                .await;
                            }
                            "pending_ticket" => {
                                set_status(&sessions, &session_id, QrStatus::Scanned)
                                    .await;
                            }
                            "pending_login" => {
                                let ticket = payload
                                    .get("ticket")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();

                                if ticket.is_empty() {
                                    set_status(
                                        &sessions,
                                        &session_id,
                                        QrStatus::Error {
                                            message: "Empty ticket".into(),
                                        },
                                    )
                                    .await;
                                    break;
                                }

                                set_status(
                                    &sessions,
                                    &session_id,
                                    QrStatus::Completing,
                                )
                                .await;

                                match finalize_with_ticket(
                                    &ticket,
                                    &private_key,
                                    &pool,
                                )
                                .await
                                {
                                    Ok(auth) => {
                                        set_status(
                                            &sessions,
                                            &session_id,
                                            QrStatus::Completed { auth },
                                        )
                                        .await;
                                    }
                                    Err(msg) => {
                                        set_status(
                                            &sessions,
                                            &session_id,
                                            QrStatus::Error { message: msg },
                                        )
                                        .await;
                                    }
                                }
                                break;
                            }
                            "finish" => {
                                if let Some(enc_token) = payload
                                    .get("encrypted_token")
                                    .and_then(|v| v.as_str())
                                {
                                    set_status(
                                        &sessions,
                                        &session_id,
                                        QrStatus::Completing,
                                    )
                                    .await;
                                    match decrypt_and_login(
                                        enc_token,
                                        &private_key,
                                        &pool,
                                    )
                                    .await
                                    {
                                        Ok(auth) => {
                                            set_status(
                                                &sessions,
                                                &session_id,
                                                QrStatus::Completed { auth },
                                            )
                                            .await;
                                        }
                                        Err(msg) => {
                                            set_status(
                                                &sessions,
                                                &session_id,
                                                QrStatus::Error { message: msg },
                                            )
                                            .await;
                                        }
                                    }
                                    break;
                                }
                            }
                            "cancel" => {
                                set_status(
                                    &sessions,
                                    &session_id,
                                    QrStatus::Cancelled,
                                )
                                .await;
                                break;
                            }
                            _ => {}
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        let is_done = {
                            let map = sessions.lock().await;
                            map.get(&session_id)
                                .map(|s| {
                                    matches!(
                                        s.status,
                                        QrStatus::Completed { .. }
                                            | QrStatus::Cancelled
                                            | QrStatus::Error { .. }
                                    )
                                })
                                .unwrap_or(true)
                        };
                        if !is_done {
                            set_status(
                                &sessions,
                                &session_id,
                                QrStatus::Error {
                                    message: "WebSocket closed by Discord".into(),
                                },
                            )
                            .await;
                        }
                        break;
                    }
                    Some(Err(e)) => {
                        set_status(
                            &sessions,
                            &session_id,
                            QrStatus::Error {
                                message: format!("WebSocket error: {e}"),
                            },
                        )
                        .await;
                        break;
                    }
                    _ => {} // Ping, Pong, Binary — ignore
                }
            }
        }
    }

    // Cleanup heartbeat
    if let Some(h) = heartbeat_handle {
        h.abort();
    }
}

// ── Token helpers ───────────────────────────────────────

async fn decrypt_and_login(
    encrypted_token_b64: &str,
    private_key: &RsaPrivateKey,
    pool: &SqlitePool,
) -> Result<serde_json::Value, String> {
    let encrypted = general_purpose::STANDARD
        .decode(encrypted_token_b64)
        .map_err(|e| format!("Token base64 decode error: {e}"))?;

    let padding = Oaep::new::<Sha256>();
    let decrypted = private_key
        .decrypt(padding, &encrypted)
        .map_err(|e| format!("Token decrypt error: {e}"))?;

    let discord_token = String::from_utf8(decrypted)
        .map_err(|_| "Token is not valid UTF-8".to_string())?
        .trim_matches('\0')
        .trim()
        .to_string();

    if discord_token.is_empty() {
        return Err("Empty token after decryption".into());
    }

    let auth = crate::auth::do_discord_token_login(pool, &discord_token)
        .await
        .map_err(|e| format!("Login failed: {e}"))?;

    Ok(serde_json::to_value(auth).unwrap_or_default())
}

async fn finalize_with_ticket(
    ticket: &str,
    private_key: &RsaPrivateKey,
    pool: &SqlitePool,
) -> Result<serde_json::Value, String> {
    let client = reqwest::Client::new();
    let resp = client
        .post(DISCORD_REMOTE_AUTH_LOGIN_API)
        .header("Content-Type", "application/json")
        .header("Origin", "https://discord.com")
        .header("User-Agent", USER_AGENT)
        .json(&serde_json::json!({ "ticket": ticket }))
        .send()
        .await
        .map_err(|e| format!("Ticket finalization error: {e}"))?;

    if !resp.status().is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("Discord rejected ticket: {text}"));
    }

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("Bad Discord response: {e}"))?;

    if let Some(enc) = body.get("encrypted_token").and_then(|v| v.as_str()) {
        return decrypt_and_login(enc, private_key, pool).await;
    }

    if let Some(tok) = body.get("token").and_then(|v| v.as_str()) {
        let t = tok.trim();
        if !t.is_empty() {
            let auth = crate::auth::do_discord_token_login(pool, t)
                .await
                .map_err(|e| format!("Login failed: {e}"))?;
            return Ok(serde_json::to_value(auth).unwrap_or_default());
        }
    }

    Err("No token in Discord finalization response".into())
}
