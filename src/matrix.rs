// ============================================================================
// ULTRACLAW — matrix.rs
// ============================================================================
// The Matrix Event Multiplexer — the central nervous system of Ultraclaw.
//
// This module:
// 1. Logs into the Matrix homeserver
// 2. Registers an event handler for room messages
// 3. For each incoming message:
//    a. Strips HTML/rich formatting to raw text (saves tokens)
//    b. Detects the source platform via room_id
//    c. Gets or creates a session for the room
//    d. Loads conversation context from the DB
//    e. Recalls relevant long-term memories
//    f. Builds the system prompt via Soul
//    g. Calls the InferenceEngine (with failover)
//    h. Parses and executes any tool calls
//    i. Formats the response for the target platform
//    j. Sends the reply back through Matrix
//
// WHY ONE MATRIX BOT REPLACES 15+ PLATFORM BOTS:
// Traditional multi-platform agents need a separate bot/SDK for each platform:
// - WhatsApp Business API SDK (~30MB + runtime memory)
// - Discord.js (~20MB + runtime memory)
// - Slack Bolt SDK (~15MB + runtime memory)
// - Telegram Bot API (~10MB + runtime memory)
// - ... × 15 platforms = 200-500MB of platform SDKs alone
//
// Ultraclaw replaces ALL of these with a single Matrix SDK (~5MB).
// The Matrix homeserver runs bridges (mautrix-whatsapp, mautrix-telegram, etc.)
// that translate between protocols. The agent sees all platforms as Matrix
// rooms and speaks a single protocol. Total SDK overhead: ~5MB vs ~500MB.
//
// MEMORY OPTIMIZATION:
// - The event handler closure captures only Arc references (8 bytes each).
// - Message processing is fully streaming: we don't accumulate messages.
// - Each event is processed and dropped before the next one is handled.
//
// ENERGY OPTIMIZATION:
// - The event loop is async. When no messages arrive, the tokio runtime
//   sleeps (epoll_wait on Linux, kqueue on macOS, IOCP on Windows).
//   Zero CPU usage during idle periods.
// - The sync loop (`sync()`) uses Matrix long-polling, which is a single
//   HTTP connection that the server holds open until there's an event.
//   No repeated polling requests burning bandwidth and CPU.
// ============================================================================

use crate::config::Config;
use crate::db::{ChatMessage, ConversationDb};
use crate::formatter;
use crate::inference::InferenceEngine;
use crate::memory::MemoryStore;
use crate::mcp::McpClient;
use crate::session::SessionManager;
use crate::skill::SkillRegistry;
use crate::soul::Soul;
use crate::tools;

use matrix_sdk::ruma::events::room::member::StrippedRoomMemberEvent;
use matrix_sdk::{
    config::SyncSettings,
    room::Room,
    ruma::{
        events::room::message::{
            MessageType, OriginalSyncRoomMessageEvent, RoomMessageEventContent,
        },
        OwnedUserId,
    },
    Client,
};
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{error, info, warn};

/// Log into the Matrix homeserver and return the authenticated client.
///
/// # Authentication Flow
/// 1. Build client with homeserver URL (resolves .well-known if needed)
/// 2. Authenticate with username/password
/// 3. The client stores the access token in memory for subsequent requests
///
/// # Memory Usage
/// The Client struct is internally Arc'd. Multiple clones share the same
/// underlying HTTP client, session state, and connection pool.
/// Total overhead: ~1-2KB for the client + connection pool.
pub async fn login(config: &Config) -> Result<Client, String> {
    let client = Client::builder()
        .homeserver_url(&config.homeserver_url)
        .build()
        .await
        .map_err(|e| format!("Failed to build Matrix client: {}", e))?;

    // Parse the user ID
    let user_id: OwnedUserId = config
        .matrix_user
        .parse()
        .map_err(|e| format!("Invalid Matrix user ID '{}': {}", config.matrix_user, e))?;

    // Login with password. The SDK handles the /login endpoint automatically.
    // The access token is stored in memory (no disk persistence in this build).
    client
        .matrix_auth()
        .login_username(user_id.localpart(), &config.matrix_password)
        .initial_device_display_name("Ultraclaw Agent")
        .await
        .map_err(|e| format!("Matrix login failed: {}", e))?;

    info!(
        user = %config.matrix_user,
        homeserver = %config.homeserver_url,
        "Matrix login successful"
    );

    Ok(client)
}

/// Run the main event loop — listen for messages and respond.
///
/// This function never returns (runs forever). It attaches an event handler
/// for room messages and then enters the Matrix sync loop.
///
/// # Shared State
/// All stateful components are wrapped in Arc<Mutex<>> for safe sharing
/// across the async event handler. The Mutex is tokio's async Mutex,
/// which yields to the scheduler while waiting (no busy-spinning).
///
/// # Architecture: Why Arc<Mutex<T>> Instead of Channels?
/// Channels (mpsc) would require a separate consumer task for each component
/// (DB writer, memory writer, session manager). That's 5+ extra tasks, each
/// with its own stack (~8KB per task = 40KB). Our approach: one handler task,
/// multiple Arc<Mutex<T>> references. Contention is negligible because
/// each message takes ~100-2000ms to process, and SQLite ops take ~1ms.
pub async fn run_event_loop(
    client: Client,
    engine: Arc<dyn InferenceEngine>,
    db: Arc<Mutex<ConversationDb>>,
    memory: Arc<Mutex<MemoryStore>>,
    sessions: Arc<Mutex<SessionManager>>,
    soul: Arc<Soul>,
    skills: Arc<SkillRegistry>,
    mcp: Option<Arc<McpClient>>,
    config: Arc<Config>,
) {
    // Get the bot's own user ID to ignore self-messages
    let bot_user_id = client.user_id().map(|id| id.to_string()).unwrap_or_default();

    // Clone Arcs for the event handler closure.
    // Each clone is just incrementing a reference count (8 bytes, atomic).
    let engine = engine.clone();
    let db = db.clone();
    let memory = memory.clone();
    let sessions = sessions.clone();
    let soul = soul.clone();
    let skills = skills.clone();
    let mcp = mcp.clone();
    let config = config.clone();
    let bot_id = bot_user_id.clone();

    // --- AUTO-JOIN HANDLER ---
    // Listens for invites and accepts them automatically.
    client.add_event_handler(
        |event: StrippedRoomMemberEvent, room: Room| async move {
            if event.content.membership == matrix_sdk::ruma::events::room::member::MembershipState::Invite {
                info!("Auto-joining room {}", room.room_id());
                if let Err(e) = room.join().await {
                    error!("Failed to auto-join room: {}", e);
                }
            }
        },
    );

    // Register the message event handler
    client.add_event_handler(
        move |event: OriginalSyncRoomMessageEvent, room: Room| {
            // Clone all Arcs again for this specific invocation.
            // Still just reference count increments — zero data copying.
            let engine = engine.clone();
            let db = db.clone();
            let memory = memory.clone();
            let sessions = sessions.clone();
            let soul = soul.clone();
            let skills = skills.clone();
            let mcp = mcp.clone();
            let config = config.clone();
            let bot_id = bot_id.clone();

            async move {


                // --- IGNORE SELF-MESSAGES ---
                if event.sender.to_string() == bot_id {
                    return;
                }

                // --- ROBUSTNESS: IGNORE SERVER/SYSTEM MESSAGES ---
                // 1. Ignore messages from the server itself (e.g. @server:domain.com)
                if event.sender.as_str().starts_with("@server:") || event.sender.as_str() == "@matrixbot:matrix.org" {
                     info!(sender = %event.sender, "Ignoring server/system message");
                     return;
                }

                // --- EXTRACT MESSAGE TEXT ---
                let body = match &event.content.msgtype {
                    MessageType::Text(text) => {
                        // ROBUSTNESS: Ignore m.notice (used by bots/bridges for alerts)
                        if matches!(event.content.msgtype, MessageType::Notice(_)) {
                            return;
                        }

                        if let Some(formatted) = &text.formatted {
                            formatter::strip_html(&formatted.body)
                        } else {
                            text.body.clone()
                        }
                    }
                    _ => return, // Ignore non-text
                };

                let room_id_str = room.room_id().to_string();
                info!(
                    room_id = %room_id_str,
                    sender = %event.sender,
                    body_len = body.len(),
                    "Received message"
                );

                // ... (Rest of processing) ...
                
                // --- DETECT PLATFORM ---
                let platform = formatter::detect_platform(&room_id_str);
                
                // --- SESSION MANAGEMENT ---
                let session_context = {
                    let mut sessions = sessions.lock().await;
                    sessions.get_or_create(&room_id_str, platform);
                    sessions.get_session_context(&room_id_str)
                };

                // --- STORE USER MESSAGE IN CONTEXT DB ---
                {
                    let db = db.lock().await;
                    if let Err(e) = db.append_message(&room_id_str, "user", &body) {
                        error!(error = %e, "Failed to store user message");
                    }
                }

                // --- LOAD CONVERSATION CONTEXT ---
                let context = {
                    let db = db.lock().await;
                    db.get_context(&room_id_str, config.context_window_size)
                        .unwrap_or_default()
                };

                // --- RECALL LONG-TERM MEMORIES ---
                let memory_context = {
                    let mem = memory.lock().await;
                    mem.summarize_for_context(&room_id_str, 200)
                        .unwrap_or(None)
                };

                // --- BUILD SYSTEM PROMPT ---
                let platform_desc = format!("User is on {:?}", platform);
                let system_msg = soul.build_system_message(
                    Some(&platform_desc),
                    session_context.as_deref(),
                    memory_context.as_deref(),
                );

                // --- BUILD FULL MESSAGE ARRAY ---
                let mut messages = Vec::with_capacity(context.len() + 2);
                messages.push(ChatMessage {
                    role: "system".to_string(),
                    content: system_msg,
                });
                messages.extend(context);

                // --- INFERENCE ---
                let tool_schema = skills.to_tool_schema();
                let response = match engine
                    .infer(
                        messages.clone(),
                        Some(tool_schema.clone()),
                        soul.temperature,
                        soul.max_tokens,
                    )
                    .await
                {
                    Ok(resp) => resp,
                    Err(e) => {
                        error!(error = %e, "All inference engines failed");
                        "I'm sorry, I'm currently unable to process your request.".to_string()
                    }
                };

                // --- TOOL EXECUTION ---
                let tool_calls = tools::parse_tool_calls(&response);
                let final_response = if !tool_calls.is_empty() {
                    info!(count = tool_calls.len(), "Executing tool calls");
                    let tool_output = tools::execute_tool_calls(&tool_calls, &skills, mcp.as_deref()).await;
                    
                    messages.push(ChatMessage { role: "assistant".to_string(), content: response.clone() });
                    messages.push(ChatMessage { role: "system".to_string(), content: format!("Tool execution results:\n{}", tool_output) });

                    match engine.infer(messages.clone(), None, soul.temperature, soul.max_tokens).await {
                        Ok(resp) => resp,
                        Err(_) => response,
                    }
                } else {
                    response
                };

                // --- FORMAT FOR PLATFORM ---
                let formatted = formatter::format_response(&final_response, platform);

                // --- STORE ASSISTANT RESPONSE ---
                {
                    let db = db.lock().await;
                    if let Err(e) = db.append_message(&room_id_str, "assistant", &formatted) {
                        warn!(error = %e, "Failed to store assistant response");
                    }
                }

                // --- SEND REPLY (ROBUSTNESS: 403 HANDLING) ---
                let content = RoomMessageEventContent::text_plain(&formatted);
                match room.send(content).await {
                    Ok(_) => info!(room_id = %room_id_str, "Reply sent successfully"),
                    Err(e) => {
                        // Check for 403 Forbidden (e.g. read-only rooms, kicked, etc.)
                        let error_msg = e.to_string();
                        if error_msg.contains("403") || error_msg.contains("M_FORBIDDEN") {
                            warn!(
                                room_id = %room_id_str, 
                                error = %e, 
                                "Permission denied (403). Ignoring to prevent crash loop."
                            );
                        } else {
                            error!(error = %e, "Failed to send reply to Matrix room");
                        }
                    }
                }
                
                // --- UPLOAD GENERATED MEDIA (if any) ---
                for tc in &tool_calls {
                    if tc.name == "generate_image" || tc.name == "generate_video" {
                        if let Some(skill_out) = skills.dispatch(tc) {
                            if !skill_out.is_error {
                                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&skill_out.output) {
                                    if let Some(file_path) = parsed["file_path"].as_str() {
                                        let path = std::path::Path::new(file_path);
                                        if path.exists() {
                                             let mime_str = parsed["mime_type"].as_str().unwrap_or("application/octet-stream");
                                             let content_type: mime::Mime = mime_str.parse().unwrap_or(mime::APPLICATION_OCTET_STREAM);
                                             let filename = path.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_else(|| "media".to_string());
                                             
                                             let attachment_config = matrix_sdk::attachment::AttachmentConfig::new();
                                             // Read file content
                                             if let Ok(data) = std::fs::read(path) {
                                                 if let Err(e) = room.send_attachment(&filename, &content_type, data, attachment_config).await {
                                                     error!(error = %e, "Failed to upload media");
                                                 }
                                             }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        },
    );

    // --- ENTER SYNC LOOP ---
    // This is the Matrix long-polling loop. The client sends a /sync request
    // to the homeserver, which blocks until there are new events (or timeout).
    //
    // Energy impact: during idle periods, the only activity is maintaining
    // one TCP connection (keepalive packets every ~30s, ~64 bytes each).
    // CPU usage: effectively zero (epoll_wait blocks the thread).
    info!("Starting Matrix sync loop (listening for messages)...");

    let settings = SyncSettings::default().timeout(std::time::Duration::from_secs(30));

    // Perform an initial sync to get the current state
    if let Err(e) = client.sync_once(settings.clone()).await {
        error!(error = %e, "Initial sync failed");
    }

    // Enter the infinite sync loop
    client.sync(settings).await.map_err(|e| {
        error!(error = %e, "Sync loop terminated with error");
    }).ok();
}
