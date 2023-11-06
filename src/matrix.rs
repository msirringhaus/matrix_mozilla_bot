use super::{LoginData, SharedState};
use matrix_sdk::{
    config::SyncSettings,
    event_handler::Ctx,
    matrix_auth::MatrixSession,
    room::Room,
    ruma::{
        events::room::member::StrippedRoomMemberEvent,
        events::room::message::{
            MessageType, OriginalSyncRoomMessageEvent, RoomMessageEventContent,
            TextMessageEventContent,
        },
    },
    Client, RoomState,
};
use std::path::Path;
use tokio::fs;
use tokio::time::{sleep, Duration};

async fn on_room_message(
    event: OriginalSyncRoomMessageEvent,
    room: Room,
    client: Client,
    ctx: Ctx<SharedState>,
) -> anyhow::Result<()> {
    if room.state() == RoomState::Joined {
        if ctx.cfg.ignore_own_messages && Some(event.sender.as_ref()) == client.user_id() {
            // Our own message, skipping.
            println!("Skipping message from ourselves.");
            return Ok(());
        }
        if ctx.cfg.accept_commands_from.is_empty()
            || ctx.cfg.accept_commands_from.contains(&event.sender)
        {
            if let MessageType::Text(TextMessageEventContent { body, .. }) = event.content.msgtype {
                if body == "!ping" {
                    let content = RoomMessageEventContent::text_plain("pong");
                    room.send(content).await?;
                }
                if body == "!leave" {
                    let content = RoomMessageEventContent::text_plain("Bye");
                    room.send(content).await?;
                    room.leave().await?;
                }
                if body == "!watch" {
                    let content = RoomMessageEventContent::text_plain("Watching...");
                    room.send(content).await?;
                    ctx.rooms.lock().unwrap().insert(room.room_id().to_owned());
                }
            }
        }
    }
    Ok(())
}

async fn on_stripped_state_member(
    room_member: StrippedRoomMemberEvent,
    client: Client,
    room: Room,
    ctx: Ctx<SharedState>,
) {
    if room_member.state_key != client.user_id().unwrap() {
        return;
    }

    if room.state() == RoomState::Invited {
        tokio::spawn(async move {
            if ctx.cfg.accept_commands_from.is_empty()
                || ctx.cfg.accept_commands_from.contains(&room_member.sender)
            {
                println!("Autojoining room {}", room.room_id());
                let mut delay = 2;

                while let Err(err) = room.join().await {
                    // retry autojoin due to synapse sending invites, before the
                    // invited user can join for more information see
                    // https://github.com/matrix-org/synapse/issues/4345
                    eprintln!(
                        "Failed to join room {} ({err:?}), retrying in {delay}s",
                        room.room_id()
                    );

                    sleep(Duration::from_secs(delay)).await;
                    delay *= 2;

                    if delay > 3600 {
                        eprintln!("Can't join room {} ({err:?})", room.room_id());
                        break;
                    }
                }
                println!("Successfully joined room {}", room.room_id());
            } else {
                println!("Rejecting invite to room {}", room.room_id());
                let mut delay = 2;
                while let Err(err) = room.leave().await {
                    // retry autojoin due to synapse sending invites, before the
                    // invited user can join for more information see
                    // https://github.com/matrix-org/synapse/issues/4345
                    eprintln!(
                        "Failed to reject room {} ({err:?}), retrying in {delay}s",
                        room.room_id()
                    );

                    sleep(Duration::from_secs(delay)).await;
                    delay *= 2;

                    if delay > 3600 {
                        eprintln!("Can't reject room {} ({err:?})", room.room_id());
                        break;
                    }
                    println!(
                        "Rejected invite from unknown user: {:?}",
                        room_member.sender
                    );
                }
            }
        });
    }
}

/// Restore a previous session.
pub async fn restore_session(client: &Client, session_file: &Path) -> anyhow::Result<()> {
    // The session was serialized as JSON in a file.
    let serialized_session = fs::read_to_string(session_file).await?;
    let user_session: MatrixSession = serde_json::from_str(&serialized_session)?;

    println!("Restoring session for {}…", user_session.meta.user_id);

    // Restore the Matrix user session.
    client.restore_session(user_session).await?;

    Ok(())
}

pub async fn login(client: &Client, aio: &SharedState) -> anyhow::Result<()> {
    match &aio.cfg.login_data {
        LoginData::UsernamePassword(username, password) => {
            client
                .matrix_auth()
                .login_username(username, password)
                .initial_device_display_name("Mozilla FTP watcher")
                .send()
                .await?;
            println!("logged in as {}", username);
        }
        #[cfg(feature = "sso-login")]
        LoginData::Sso => {
            let response = client
                .matrix_auth()
                .login_sso(|sso_url| async move {
                    // Open sso_url
                    println!("{sso_url}");
                    Ok(())
                })
                .initial_device_display_name("Mozilla FTP watcher")
                .send()
                .await
                .unwrap();

            println!(
                "Logged in as {}, got device_id {} and access_token {}",
                response.user_id, response.device_id, response.access_token
            );
        }
    }
    Ok(())
}

pub async fn login_and_sync(aio: SharedState) -> anyhow::Result<Client> {
    let mut client_builder = Client::builder().homeserver_url(aio.cfg.homeserver_url.clone());
    if let Some(db) = &aio.cfg.session_storage.get_session_db() {
        client_builder = client_builder.sqlite_store(&db.db_path, Some(&db.db_pw));
    }

    let client = client_builder.build().await?;
    let mut logged_in = false;
    if aio.cfg.session_storage.session_store_exists() {
        logged_in = restore_session(
            &client,
            &aio.cfg.session_storage.get_session_store().unwrap(),
        )
        .await
        .is_ok();
    }

    if !logged_in {
        login(&client, &aio).await?;
        match &aio.cfg.session_storage {
            crate::SessionStorage::Ephemeral => todo!(),
            crate::SessionStorage::Plain(_, session) => {
                let user_session = client
                    .matrix_auth()
                    .session()
                    .expect("A logged-in client should have a session");
                let serialized_session = serde_json::to_string(&user_session)?;
                fs::write(&session.session_path, serialized_session).await?;
            }
        }
    }

    // An initial sync to set up state and so our bot doesn't respond to old
    // messages. If the `StateStore` finds saved state in the location given the
    // initial sync will be skipped in favor of loading state from the store
    let response = client.sync_once(SyncSettings::default()).await?;
    // add our CommandBot to be notified of incoming messages, we do this after the
    // initial sync to avoid responding to messages before the bot was running.
    client.add_event_handler_context(aio.clone());
    if aio.cfg.autojoin {
        client.add_event_handler(on_stripped_state_member);
    }
    client.add_event_handler(on_room_message);

    let client_cc = client.clone();
    // since we called `sync_once` before we entered our sync loop we must pass
    // that sync token to `sync`
    let settings = SyncSettings::default().token(response.next_batch);
    // this keeps state from the server streaming in to CommandBot via the
    // EventHandler trait
    tokio::spawn(async move { client.sync(settings).await });

    Ok(client_cc)
}
