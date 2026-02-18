// ═══════════════════════════════════════════════════════
//  Voxium — Discord Gateway (per-user) for voice joining
// ═══════════════════════════════════════════════════════
//
// This module manages per-user Discord Gateway WebSocket connections.
// It is used to send Update Voice State (op 4) so the user can join
// Discord voice channels. The gateway returns VOICE_STATE_UPDATE and
// VOICE_SERVER_UPDATE events which contain the information needed to
// connect to the Discord Voice Gateway.

use actix_web::{web, HttpRequest, HttpResponse};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_tungstenite::tungstenite::Message;

use crate::auth::extract_claims;

const DISCORD_GATEWAY_URL: &str = "wss://gateway.discord.gg/?v=9&encoding=json";

// ── Types ───────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceServerInfo {
    pub token: String,
    pub endpoint: Option<String>,
    pub guild_id: Option<String>,
    pub session_id: String,
    pub user_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct VoiceParticipant {
    pub user_id: String,
    pub channel_id: Option<String>,
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct VoiceJoinPayload {
    pub guild_id: String,
    pub channel_id: String,
}

#[derive(Debug, Deserialize)]
pub struct VoiceLeavePayload {
    pub guild_id: String,
}

// Commands sent from HTTP handlers to the gateway task
#[derive(Debug)]
enum GatewayCommand {
    JoinVoice {
        guild_id: String,
        channel_id: String,
        reply: oneshot::Sender<Result<VoiceServerInfo, String>>,
    },
    LeaveVoice {
        guild_id: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
}

pub(crate) struct GatewaySession {
    cmd_tx: mpsc::Sender<GatewayCommand>,
    presence: Arc<Mutex<VoicePresenceState>>,
}

pub type DiscordGateways = Arc<Mutex<HashMap<String, GatewaySession>>>;

pub fn create_discord_gateways() -> DiscordGateways {
    Arc::new(Mutex::new(HashMap::new()))
}

#[derive(Default)]
struct VoicePresenceState {
    // guild_id -> user_id -> participant
    by_guild: HashMap<String, HashMap<String, VoiceParticipant>>,
}

// ── Gateway task ────────────────────────────────────────

async fn run_gateway(
    discord_token: String,
    mut cmd_rx: mpsc::Receiver<GatewayCommand>,
    presence: Arc<Mutex<VoicePresenceState>>,
) {
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::http::HeaderValue;

    let mut request = match DISCORD_GATEWAY_URL.into_client_request() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[discord-gw] Failed to build request: {e}");
            return;
        }
    };
    request.headers_mut().insert("Origin", HeaderValue::from_static("https://discord.com"));
    request.headers_mut().insert(
        "User-Agent",
        HeaderValue::from_static("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36"),
    );

    eprintln!("[discord-gw] Connecting to Discord Gateway...");
    let connect_result = connect_async(request).await;
    let (ws_stream, _) = match connect_result {
        Ok(r) => {
            eprintln!("[discord-gw] Connected to Discord Gateway");
            r
        }
        Err(e) => {
            eprintln!("[discord-gw] Connection failed: {e}");
            // Drain any pending commands
            while let Some(cmd) = cmd_rx.recv().await {
                match cmd {
                    GatewayCommand::JoinVoice { reply, .. } => {
                        let _ = reply.send(Err("Gateway connection failed".into()));
                    }
                    GatewayCommand::LeaveVoice { reply, .. } => {
                        let _ = reply.send(Err("Gateway connection failed".into()));
                    }
                }
            }
            return;
        }
    };

    let (mut ws_tx, mut ws_rx) = ws_stream.split();

    // State
    let mut heartbeat_interval_ms: u64 = 41250;
    let mut sequence: Option<u64> = None;
    let mut session_id: Option<String> = None;
    let mut identified = false;
    let mut pending_voice_join: Option<(
        String, // guild_id
        String, // channel_id
        oneshot::Sender<Result<VoiceServerInfo, String>>,
    )> = None;
    // Queued join command waiting for READY event
    let mut queued_join: Option<GatewayCommand> = None;
    let mut voice_token: Option<String> = None;
    let mut voice_endpoint: Option<String> = None;
    let mut voice_guild_id: Option<String> = None;
    let mut discord_user_id: Option<String> = None;

    // Heartbeat ticker
    let (hb_tx, mut hb_rx) = mpsc::channel::<()>(1);

    let mut running = true;

    while running {
        tokio::select! {
            // Receive from Discord Gateway
            msg = ws_rx.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        let payload: serde_json::Value = match serde_json::from_str(&text) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };

                        let op = payload.get("op").and_then(|v| v.as_u64()).unwrap_or(999);

                        // Update sequence
                        if let Some(s) = payload.get("s").and_then(|v| v.as_u64()) {
                            sequence = Some(s);
                        }

                        match op {
                            // 10 = Hello
                            10 => {
                                if let Some(interval) = payload
                                    .get("d")
                                    .and_then(|d| d.get("heartbeat_interval"))
                                    .and_then(|v| v.as_u64())
                                {
                                    heartbeat_interval_ms = interval;
                                }

                                // Start heartbeat loop
                                let hb_interval = heartbeat_interval_ms;
                                let hb_tx_clone = hb_tx.clone();
                                tokio::spawn(async move {
                                    let mut interval = tokio::time::interval(
                                        std::time::Duration::from_millis(hb_interval),
                                    );
                                    loop {
                                        interval.tick().await;
                                        if hb_tx_clone.send(()).await.is_err() {
                                            break;
                                        }
                                    }
                                });

                                // Send Identify
                                if !identified {
                                    // Intents: GUILDS (1) + GUILD_VOICE_STATES (1<<7=128) = 129
                                    let identify = serde_json::json!({
                                        "op": 2,
                                        "d": {
                                            "token": discord_token,
                                            "capabilities": 30717,
                                            "properties": {
                                                "os": "Windows",
                                                "browser": "Chrome",
                                                "device": "",
                                                "system_locale": "fr-FR",
                                                "browser_user_agent": "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
                                                "browser_version": "131.0.0.0",
                                                "os_version": "10",
                                                "referrer": "",
                                                "referring_domain": "",
                                                "referrer_current": "",
                                                "referring_domain_current": "",
                                                "release_channel": "stable",
                                                "client_build_number": 366068,
                                                "client_event_source": serde_json::Value::Null
                                            },
                                            "presence": {
                                                "activities": [],
                                                "status": "online",
                                                "since": 0,
                                                "afk": false
                                            },
                                            "compress": false,
                                            "client_state": {
                                                "guild_versions": {},
                                                "highest_last_message_id": "0",
                                                "read_state_version": 0,
                                                "user_guild_settings_version": -1,
                                                "user_settings_version": -1,
                                                "private_channels_version": "0",
                                                "api_code_version": 0
                                            }
                                        }
                                    });
                                    eprintln!("[discord-gw] Sending Identify");
                                    let _ = ws_tx.send(Message::Text(identify.to_string())).await;
                                    identified = true;
                                }
                            }

                            // 11 = Heartbeat ACK
                            11 => {
                                // OK
                            }

                            // 0 = Dispatch
                            0 => {
                                let event_name = payload.get("t").and_then(|v| v.as_str()).unwrap_or("");
                                let d = payload.get("d");

                                match event_name {
                                    "READY" | "READY_SUPPLEMENTAL" => {
                                        if event_name == "READY" {
                                            if let Some(data) = d {
                                                session_id = data.get("session_id")
                                                    .and_then(|v| v.as_str())
                                                    .map(|s| s.to_string());
                                                discord_user_id = data.get("user")
                                                    .and_then(|u| u.get("id"))
                                                    .and_then(|v| v.as_str())
                                                    .map(|s| s.to_string());
                                                eprintln!("[discord-gw] READY — session_id={:?} user_id={:?}", session_id, discord_user_id);
                                            }
                                        } else {
                                            eprintln!("[discord-gw] READY_SUPPLEMENTAL received");
                                        }

                                        // Process any queued join command
                                        if let Some(GatewayCommand::JoinVoice { guild_id, channel_id, reply }) = queued_join.take() {
                                            voice_token = None;
                                            voice_endpoint = None;
                                            voice_guild_id = None;
                                            pending_voice_join = Some((guild_id.clone(), channel_id.clone(), reply));

                                            eprintln!("[discord-gw] Processing queued join: guild={guild_id} channel={channel_id}");

                                            let voice_state = serde_json::json!({
                                                "op": 4,
                                                "d": {
                                                    "guild_id": guild_id,
                                                    "channel_id": channel_id,
                                                    "self_mute": false,
                                                    "self_deaf": false,
                                                    "self_video": false
                                                }
                                            });
                                            let _ = ws_tx.send(Message::Text(voice_state.to_string())).await;
                                        }
                                    }

                                    "VOICE_STATE_UPDATE" => {
                                        if let Some(data) = d {
                                            // Update presence cache for UI (all users)
                                            let guild_id = data.get("guild_id").and_then(|v| v.as_str()).unwrap_or("");
                                            let channel_id = data.get("channel_id").and_then(|v| v.as_str()).map(|s| s.to_string());
                                            let event_user_id = data.get("user_id")
                                                .and_then(|v| v.as_str())
                                                .or_else(|| data.get("member").and_then(|m| m.get("user")).and_then(|u| u.get("id")).and_then(|v| v.as_str()))
                                                .unwrap_or("");

                                            if !guild_id.is_empty() && !event_user_id.is_empty() {
                                                let display_name = data
                                                    .get("member")
                                                    .and_then(|m| m.get("nick"))
                                                    .and_then(|v| v.as_str())
                                                    .map(|s| s.to_string())
                                                    .or_else(|| {
                                                        data.get("member")
                                                            .and_then(|m| m.get("user"))
                                                            .and_then(|u| u.get("global_name"))
                                                            .and_then(|v| v.as_str())
                                                            .map(|s| s.to_string())
                                                    })
                                                    .or_else(|| {
                                                        data.get("member")
                                                            .and_then(|m| m.get("user"))
                                                            .and_then(|u| u.get("username"))
                                                            .and_then(|v| v.as_str())
                                                            .map(|s| s.to_string())
                                                    });

                                                let avatar_hash = data
                                                    .get("member")
                                                    .and_then(|m| m.get("user"))
                                                    .and_then(|u| u.get("avatar"))
                                                    .and_then(|v| v.as_str())
                                                    .map(|s| s.to_string());

                                                let avatar_url = avatar_hash.map(|hash| {
                                                    format!("https://cdn.discordapp.com/avatars/{}/{}.png?size=64", event_user_id, hash)
                                                });

                                                let mut p = presence.lock().await;
                                                let guild_map = p.by_guild.entry(guild_id.to_string()).or_default();
                                                if channel_id.is_none() {
                                                    guild_map.remove(event_user_id);
                                                } else {
                                                    guild_map.insert(
                                                        event_user_id.to_string(),
                                                        VoiceParticipant {
                                                            user_id: event_user_id.to_string(),
                                                            channel_id: channel_id.clone(),
                                                            display_name,
                                                            avatar_url,
                                                        },
                                                    );
                                                }
                                            }

                                            // Check this is for our user
                                            let event_user_id = data.get("user_id")
                                                .and_then(|v| v.as_str())
                                                .or_else(|| data.get("member").and_then(|m| m.get("user")).and_then(|u| u.get("id")).and_then(|v| v.as_str()))
                                                .unwrap_or("");
                                            let our_id = discord_user_id.as_deref().unwrap_or("");

                                            eprintln!("[discord-gw] VOICE_STATE_UPDATE — event_user={} our_user={} channel={:?}",
                                                event_user_id, our_id,
                                                data.get("channel_id").and_then(|v| v.as_str()));

                                            if event_user_id == our_id {
                                                // If VOICE_SERVER_UPDATE already arrived, reply now
                                                if voice_token.is_some() && voice_endpoint.is_some() {
                                                    if let Some((_, _, reply)) = pending_voice_join.take() {
                                                        let info = VoiceServerInfo {
                                                            token: voice_token.take().unwrap_or_default(),
                                                            endpoint: voice_endpoint.take(),
                                                            guild_id: voice_guild_id.take(),
                                                            session_id: session_id.clone().unwrap_or_default(),
                                                            user_id: our_id.to_string(),
                                                        };
                                                        eprintln!("[discord-gw] Sending voice info to frontend (via VSU): endpoint={:?}", info.endpoint);
                                                        let _ = reply.send(Ok(info));
                                                    }
                                                }
                                            }
                                        }
                                    }

                                    "VOICE_SERVER_UPDATE" => {
                                        if let Some(data) = d {
                                            eprintln!("[discord-gw] VOICE_SERVER_UPDATE — endpoint={:?} guild={:?}",
                                                data.get("endpoint").and_then(|v| v.as_str()),
                                                data.get("guild_id").and_then(|v| v.as_str()));
                                            voice_token = data.get("token")
                                                .and_then(|v| v.as_str())
                                                .map(|s| s.to_string());
                                            voice_endpoint = data.get("endpoint")
                                                .and_then(|v| v.as_str())
                                                .map(|s| s.to_string());
                                            voice_guild_id = data.get("guild_id")
                                                .and_then(|v| v.as_str())
                                                .map(|s| s.to_string());

                                            // VOICE_SERVER_UPDATE + the gateway session_id from READY
                                            // is everything we need to connect to the Voice Gateway
                                            if let Some((_, _, reply)) = pending_voice_join.take() {
                                                let info = VoiceServerInfo {
                                                    token: voice_token.take().unwrap_or_default(),
                                                    endpoint: voice_endpoint.take(),
                                                    guild_id: voice_guild_id.take(),
                                                    session_id: session_id.clone().unwrap_or_default(),
                                                    user_id: discord_user_id.clone().unwrap_or_default(),
                                                };
                                                eprintln!("[discord-gw] Sending voice info to frontend: endpoint={:?}", info.endpoint);
                                                let _ = reply.send(Ok(info));
                                            }
                                        }
                                    }

                                    _ => {
                                        // Log unhandled dispatch events for debugging
                                        eprintln!("[discord-gw] Dispatch event: {} (ignored)", event_name);
                                    }
                                }
                            }

                            // 7 = Reconnect
                            7 => {
                                eprintln!("[discord-gw] Received Reconnect (op 7)");
                                running = false;
                            }

                            // 9 = Invalid Session
                            9 => {
                                eprintln!("[discord-gw] Received Invalid Session (op 9)");
                                running = false;
                                if let Some((_, _, reply)) = pending_voice_join.take() {
                                    let _ = reply.send(Err("Discord session invalid".into()));
                                }
                            }

                            _ => {}
                        }
                    }

                    Some(Ok(Message::Close(frame))) => {
                        eprintln!("[discord-gw] WS Closed: {:?}", frame);
                        running = false;
                    }
                    None => {
                        eprintln!("[discord-gw] WS stream ended");
                        running = false;
                    }

                    _ => {}
                }
            }

            // Heartbeat timer
            _ = hb_rx.recv() => {
                let hb = serde_json::json!({
                    "op": 1,
                    "d": sequence
                });
                if ws_tx.send(Message::Text(hb.to_string())).await.is_err() {
                    running = false;
                }
            }

            // Commands from HTTP handlers
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(GatewayCommand::JoinVoice { guild_id, channel_id, reply }) => {
                        if session_id.is_none() {
                            // Gateway not ready yet, queue the command
                            eprintln!("[discord-gw] Gateway not ready yet, queueing join for guild={guild_id} channel={channel_id}");
                            queued_join = Some(GatewayCommand::JoinVoice { guild_id, channel_id, reply });
                            continue;
                        }

                        // If there's a pending join, cancel it first
                        if let Some((_, _, old_reply)) = pending_voice_join.take() {
                            eprintln!("[discord-gw] Cancelling previous pending join");
                            let _ = old_reply.send(Err("Superseded by new join request".into()));
                        }

                        // First, leave any current voice channel in this guild
                        // to ensure Discord sends fresh VOICE_SERVER_UPDATE
                        eprintln!("[discord-gw] Sending leave before join for guild={guild_id}");
                        let leave_state = serde_json::json!({
                            "op": 4,
                            "d": {
                                "guild_id": &guild_id,
                                "channel_id": serde_json::Value::Null,
                                "self_mute": false,
                                "self_deaf": false
                            }
                        });
                        let _ = ws_tx.send(Message::Text(leave_state.to_string())).await;

                        // Small delay to let Discord process the leave
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

                        eprintln!("[discord-gw] Sending Voice State Update (join): guild={guild_id} channel={channel_id}");

                        // Clear previous voice state
                        voice_token = None;
                        voice_endpoint = None;
                        voice_guild_id = None;

                        // Store pending request
                        pending_voice_join = Some((guild_id.clone(), channel_id.clone(), reply));

                        // Send Update Voice State (op 4)
                        let voice_state = serde_json::json!({
                            "op": 4,
                            "d": {
                                "guild_id": guild_id,
                                "channel_id": channel_id,
                                "self_mute": false,
                                "self_deaf": false,
                                "self_video": false
                            }
                        });

                        if ws_tx.send(Message::Text(voice_state.to_string())).await.is_err() {
                            if let Some((_, _, reply)) = pending_voice_join.take() {
                                let _ = reply.send(Err("Failed to send voice state update".into()));
                            }
                        }

                        // Voice join sent; we wait for the voice events above
                    }

                    Some(GatewayCommand::LeaveVoice { guild_id, reply }) => {
                        // Send Update Voice State with channel_id: null
                        let voice_state = serde_json::json!({
                            "op": 4,
                            "d": {
                                "guild_id": guild_id,
                                "channel_id": serde_json::Value::Null,
                                "self_mute": false,
                                "self_deaf": false
                            }
                        });

                        if ws_tx.send(Message::Text(voice_state.to_string())).await.is_err() {
                            let _ = reply.send(Err("Failed to send voice leave".into()));
                        } else {
                            let _ = reply.send(Ok(()));
                        }
                    }

                    None => {
                        running = false;
                    }
                }
            }
        }
    }

    // Cleanup: close the WS and drain pending
    let _ = ws_tx.close().await;
    if let Some((_, _, reply)) = pending_voice_join.take() {
        let _ = reply.send(Err("Gateway connection closed".into()));
    }
}

// ── Ensure a gateway session exists for the user ────────

async fn ensure_gateway(
    user_id: &str,
    discord_token: &str,
    gateways: &DiscordGateways,
) -> mpsc::Sender<GatewayCommand> {
    ensure_gateway_session(user_id, discord_token, gateways)
        .await
        .0
}

async fn ensure_gateway_session(
    user_id: &str,
    discord_token: &str,
    gateways: &DiscordGateways,
) -> (mpsc::Sender<GatewayCommand>, Arc<Mutex<VoicePresenceState>>) {
    let mut map = gateways.lock().await;

    // Check if existing session is still alive
    if let Some(session) = map.get(user_id) {
        if !session.cmd_tx.is_closed() {
            return (session.cmd_tx.clone(), session.presence.clone());
        }
        // Dead session, remove it
        map.remove(user_id);
    }

    // Create new session
    let (cmd_tx, cmd_rx) = mpsc::channel(16);
    let token = discord_token.to_string();
    let presence: Arc<Mutex<VoicePresenceState>> = Arc::new(Mutex::new(VoicePresenceState::default()));
    let presence_clone = presence.clone();

    tokio::spawn(async move {
        run_gateway(token, cmd_rx, presence_clone).await;
    });

    map.insert(
        user_id.to_string(),
        GatewaySession {
            cmd_tx: cmd_tx.clone(),
            presence: presence.clone(),
        },
    );

    (cmd_tx, presence)
}

#[derive(Debug, Deserialize)]
pub struct VoiceParticipantsQuery {
    pub guild_id: String,
    pub channel_id: Option<String>,
}

/// GET /api/discord/voice/participants?guild_id=...&channel_id=...
pub async fn voice_participants(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
    gateways: web::Data<DiscordGateways>,
    query: web::Query<VoiceParticipantsQuery>,
) -> HttpResponse {
    let claims = match extract_claims(&req) {
        Some(c) => c,
        None => return HttpResponse::Unauthorized().finish(),
    };

    let discord_token = match get_discord_token(pool.get_ref(), &claims.sub).await {
        Ok(t) => t,
        Err(e) => {
            return HttpResponse::BadRequest().json(serde_json::json!({ "error": e }));
        }
    };

    let (_cmd_tx, presence) = ensure_gateway_session(&claims.sub, &discord_token, gateways.get_ref()).await;
    let p = presence.lock().await;
    let guild_map = match p.by_guild.get(&query.guild_id) {
        Some(m) => m,
        None => {
            return HttpResponse::Ok().json(Vec::<VoiceParticipant>::new());
        }
    };

    let mut participants: Vec<VoiceParticipant> = guild_map.values().cloned().collect();
    if let Some(channel_id) = query.channel_id.as_deref() {
        participants.retain(|u| u.channel_id.as_deref() == Some(channel_id));
    }

    HttpResponse::Ok().json(participants)
}

// ── Helper: get Discord token for user ──────────────────

async fn get_discord_token(pool: &SqlitePool, user_id: &str) -> Result<String, String> {
    let row = sqlx::query("SELECT discord_access_token FROM users WHERE id = ?")
        .bind(user_id)
        .fetch_optional(pool)
        .await
        .map_err(|_| "Database error".to_string())?;

    let row = row.ok_or("User not found")?;
    let token: Option<String> = row
        .try_get("discord_access_token")
        .unwrap_or(None);

    token.ok_or("No Discord token linked".to_string())
}

// ── HTTP Handlers ───────────────────────────────────────

/// POST /api/discord/voice/join
/// Body: { guild_id, channel_id }
/// Returns: VoiceServerInfo with token, endpoint, session_id, user_id
pub async fn voice_join(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
    gateways: web::Data<DiscordGateways>,
    body: web::Json<VoiceJoinPayload>,
) -> HttpResponse {
    let claims = match extract_claims(&req) {
        Some(c) => c,
        None => return HttpResponse::Unauthorized().finish(),
    };

    let discord_token = match get_discord_token(pool.get_ref(), &claims.sub).await {
        Ok(t) => t,
        Err(e) => {
            return HttpResponse::BadRequest().json(serde_json::json!({ "error": e }));
        }
    };

    let cmd_tx = ensure_gateway(&claims.sub, &discord_token, gateways.get_ref()).await;

    let (reply_tx, reply_rx) = oneshot::channel();

    if cmd_tx
        .send(GatewayCommand::JoinVoice {
            guild_id: body.guild_id.clone(),
            channel_id: body.channel_id.clone(),
            reply: reply_tx,
        })
        .await
        .is_err()
    {
        // Gateway task died, remove from map
        let mut map = gateways.lock().await;
        map.remove(&claims.sub);
        return HttpResponse::InternalServerError().json(serde_json::json!({
            "error": "Discord Gateway session lost"
        }));
    }

    // Wait for the voice server info with a timeout (20s to allow for gateway identify + voice join)
    eprintln!("[discord-gw] HTTP handler waiting for voice info (20s timeout)...");
    match tokio::time::timeout(std::time::Duration::from_secs(20), reply_rx).await {
        Ok(Ok(Ok(info))) => {
            eprintln!("[discord-gw] HTTP handler returning voice info OK — endpoint={:?}", info.endpoint);
            HttpResponse::Ok().json(info)
        }
        Ok(Ok(Err(e))) => {
            eprintln!("[discord-gw] HTTP handler returning error from gateway: {e}");
            HttpResponse::BadGateway().json(serde_json::json!({ "error": e }))
        }
        Ok(Err(_)) => {
            eprintln!("[discord-gw] HTTP handler: oneshot channel dropped");
            HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Internal channel error"
            }))
        }
        Err(_) => {
            eprintln!("[discord-gw] HTTP handler: TIMEOUT — no voice info in 20s");
            HttpResponse::GatewayTimeout().json(serde_json::json!({
                "error": "Timeout waiting for Discord voice server info"
            }))
        }
    }
}

/// POST /api/discord/voice/leave
/// Body: { guild_id }
pub async fn voice_leave(
    req: HttpRequest,
    pool: web::Data<SqlitePool>,
    gateways: web::Data<DiscordGateways>,
    body: web::Json<VoiceLeavePayload>,
) -> HttpResponse {
    let claims = match extract_claims(&req) {
        Some(c) => c,
        None => return HttpResponse::Unauthorized().finish(),
    };

    let discord_token = match get_discord_token(pool.get_ref(), &claims.sub).await {
        Ok(t) => t,
        Err(e) => {
            return HttpResponse::BadRequest().json(serde_json::json!({ "error": e }));
        }
    };

    let cmd_tx = ensure_gateway(&claims.sub, &discord_token, gateways.get_ref()).await;

    let (reply_tx, reply_rx) = oneshot::channel();

    if cmd_tx
        .send(GatewayCommand::LeaveVoice {
            guild_id: body.guild_id.clone(),
            reply: reply_tx,
        })
        .await
        .is_err()
    {
        let mut map = gateways.lock().await;
        map.remove(&claims.sub);
        return HttpResponse::InternalServerError().json(serde_json::json!({
            "error": "Discord Gateway session lost"
        }));
    }

    match tokio::time::timeout(std::time::Duration::from_secs(5), reply_rx).await {
        Ok(Ok(Ok(()))) => {
            HttpResponse::Ok().json(serde_json::json!({ "ok": true }))
        }
        Ok(Ok(Err(e))) => {
            HttpResponse::BadGateway().json(serde_json::json!({ "error": e }))
        }
        _ => HttpResponse::InternalServerError().json(serde_json::json!({
            "error": "Failed to leave voice"
        })),
    }
}
