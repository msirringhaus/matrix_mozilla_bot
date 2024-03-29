use super::{LoginData, SharedState};
use matrix_sdk::{
    config::SyncSettings,
    event_handler::Ctx,
    matrix_auth::{MatrixSession, MatrixSessionTokens},
    room::Room,
    ruma::{
        api::client::{error::ErrorKind, filter::FilterDefinition},
        events::room::member::StrippedRoomMemberEvent,
        events::room::message::{
            MessageType, OriginalSyncRoomMessageEvent, RoomMessageEventContent,
            TextMessageEventContent,
        },
        OwnedDeviceId, OwnedUserId,
    },
    Client, RoomState, SessionMeta,
};
use secret_service::{EncryptionType, SecretService};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::Path};
use tokio::fs;
use tokio::time::{sleep, Duration};

macro_rules! store_to_secret_service {
    ($collection:expr, $name:expr, $data:expr) => {
        $collection
            .create_item(
                "matrix_mozilla_bot",
                HashMap::from([("matrix_mozilla_bot", $name)]),
                $data,
                true, // replace item with same attributes
                "text/plain",
            )
            .await?;
    };
}

macro_rules! get_from_secret_service {
    ($collection:expr, $name:expr) => {
        String::from_utf8(
            $collection
                .search_items(HashMap::from([("matrix_mozilla_bot", $name)]))
                .await?
                .get(0)
                .ok_or(secret_service::Error::NoResult)?
                .get_secret()
                .await?,
        )?
    };
}

macro_rules! get_optional_from_secret_service {
    ($collection:expr, $name:expr) => {
        if let Ok(tokens) = $collection
            .search_items(HashMap::from([("name", $name)]))
            .await
        {
            // Can't use .map() here, because of async-weirdness
            if let Some(t) = tokens.get(0) {
                t.get_secret()
                    .await
                    .map(|x| String::from_utf8(x).ok())
                    .ok()
                    .flatten()
            } else {
                None
            }
        } else {
            None
        }
    };
}

async fn update_room_cache(ctx: &Ctx<SharedState>) -> anyhow::Result<()> {
    if let Some(db) = ctx.cfg.session_storage.get_session_db() {
        if db.db_path.exists() {
            let serialized_rooms = serde_json::to_string(&*ctx.rooms.lock().unwrap())?;
            fs::write(&db.db_path.join("watched_rooms"), serialized_rooms).await?;
        }
    }
    Ok(())
}

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
                    ctx.rooms.lock().unwrap().remove(room.room_id());
                    update_room_cache(&ctx).await?;
                }
                if body == "!watch" {
                    let content = RoomMessageEventContent::text_plain("Watching...");
                    room.send(content).await?;
                    ctx.rooms.lock().unwrap().insert(room.room_id().to_owned());
                    update_room_cache(&ctx).await?;
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

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PlainMatrixSession {
    user_session: MatrixSession,
    sync_token: Option<String>,
}

/// Restore a previous session from plain storage.
pub async fn restore_plain_session(
    client: &Client,
    session_file: &Path,
) -> anyhow::Result<Option<String>> {
    // The session was serialized as JSON in a file.
    let serialized_session = fs::read_to_string(session_file).await?;
    let session: PlainMatrixSession = serde_json::from_str(&serialized_session)?;

    println!(
        "Restoring session for {}…",
        session.user_session.meta.user_id
    );

    // Restore the Matrix user session.
    client.restore_session(session.user_session).await?;

    Ok(session.sync_token)
}

/// Restore a previous session via SecretService.
pub async fn restore_ss_session(client: &Client) -> anyhow::Result<Option<String>> {
    let ss = SecretService::connect(EncryptionType::Dh).await?;
    let collection = ss.get_default_collection().await?;
    let access_token = get_from_secret_service!(collection, "access_token");
    let device_id = get_from_secret_service!(collection, "device_id");
    let user_id = get_from_secret_service!(collection, "user_id");
    let refresh_token = get_optional_from_secret_service!(collection, "refresh_token");
    let sync_token = get_optional_from_secret_service!(collection, "sync_token");

    let user_session = MatrixSession {
        meta: SessionMeta {
            user_id: OwnedUserId::try_from(user_id)?,
            device_id: OwnedDeviceId::try_from(device_id)?,
        },
        tokens: MatrixSessionTokens {
            access_token,
            refresh_token,
        },
    };
    println!("Restoring session for {}…", user_session.meta.user_id);

    // Restore the Matrix user session.
    client.restore_session(user_session).await?;

    Ok(sync_token)
}

pub async fn store_plain_session(
    client: &Client,
    session_path: &Path,
    sync_token: &str,
) -> anyhow::Result<()> {
    let user_session = client
        .matrix_auth()
        .session()
        .expect("A logged-in client should have a session");
    let data = PlainMatrixSession {
        user_session,
        sync_token: Some(sync_token.to_string()),
    };
    let serialized_session = serde_json::to_string(&data)?;
    fs::write(&session_path, serialized_session).await?;
    Ok(())
}

pub async fn store_ss_session(client: &Client, sync_token: &str) -> anyhow::Result<()> {
    let user_session = client
        .matrix_auth()
        .session()
        .expect("A logged-in client should have a session");
    let ss = SecretService::connect(EncryptionType::Dh).await?;
    let collection = match ss.get_default_collection().await {
        Ok(c) => c,
        Err(secret_service::Error::NoResult) => {
            ss.create_collection("matrix_mozilla_bot", "default")
                .await?
        }
        Err(x) => {
            return Err(x.into());
        }
    };

    if let Some(refresh_token) = user_session.tokens.refresh_token {
        store_to_secret_service!(collection, "refresh_token", refresh_token.as_bytes());
    }
    store_to_secret_service!(collection, "sync_token", sync_token.as_bytes());
    store_to_secret_service!(
        collection,
        "access_token",
        user_session.tokens.access_token.as_bytes()
    );
    store_to_secret_service!(collection, "user_id", user_session.meta.user_id.as_bytes());
    store_to_secret_service!(
        collection,
        "device_id",
        user_session.meta.device_id.as_bytes()
    );
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

    let mut client = client_builder.build().await?;
    let (mut logged_in, sync_token) = match &aio.cfg.session_storage {
        crate::SessionStorage::Ephemeral => (false, None), // Nothing to restore
        crate::SessionStorage::Plain(_, session) => {
            if let Ok(sync_token) = restore_plain_session(&client, &session.session_path).await {
                (true, sync_token)
            } else {
                (false, None)
            }
        }
        crate::SessionStorage::SecretService(_) => {
            if let Ok(sync_token) = restore_ss_session(&client).await {
                (true, sync_token)
            } else {
                (false, None)
            }
        }
    };

    let filter = FilterDefinition::with_lazy_loading();
    let mut sync_settings = SyncSettings::default().filter(filter.clone().into());

    // We restore the sync where we left.
    // This is not necessary when not using `sync_once`. The other sync methods get
    // the sync token from the store.
    if let Some(sync_token) = sync_token {
        sync_settings = sync_settings.token(sync_token);
    }

    // Let's ignore messages before the program was launched.
    // This is a loop in case the initial sync is longer than our timeout. The
    // server should cache the response and it will ultimately take less time to
    // receive.
    loop {
        if !logged_in {
            login(&client, &aio).await?;
            logged_in = true;
        }
        match client.sync_once(sync_settings.clone()).await {
            Ok(response) => {
                // This is the last time we need to provide this token, the sync method after
                // will handle it on its own.
                sync_settings = sync_settings.token(response.next_batch.clone());
                match &aio.cfg.session_storage {
                    crate::SessionStorage::Ephemeral => (),
                    crate::SessionStorage::Plain(_, session) => {
                        store_plain_session(&client, &session.session_path, &response.next_batch)
                            .await?;
                    }
                    crate::SessionStorage::SecretService(..) => {
                        store_ss_session(&client, &response.next_batch).await?;
                    }
                }
                // persist_sync_token(session_file, response.next_batch).await?;
                break;
            }
            Err(error) => match error.client_api_error_kind() {
                None => {
                    println!("An error occurred during initial sync: {error}");
                    println!("Trying again…");
                }
                Some(ErrorKind::LimitExceeded { retry_after_ms }) => {
                    sleep(retry_after_ms.unwrap_or(Duration::from_secs(5))).await;
                    continue;
                }
                Some(ErrorKind::ConnectionTimeout) => {}
                Some(ErrorKind::UnknownToken { .. }) | Some(ErrorKind::MissingToken) => {
                    println!("The login data we sent didn't work (probably from restoring the session). Trying fresh login.");
                    logged_in = false;
                    // Unsetting sync_token isn't possible, so we recreate a new sync_setting-object
                    sync_settings = SyncSettings::default().filter(filter.clone().into());
                    let mut client_builder =
                        Client::builder().homeserver_url(aio.cfg.homeserver_url.clone());
                    if let Some(db) = &aio.cfg.session_storage.get_session_db() {
                        println!("Removing storage DB");
                        // We need to clear the database, too
                        tokio::fs::remove_dir_all(&db.db_path).await?;
                        client_builder = client_builder.sqlite_store(&db.db_path, Some(&db.db_pw));
                    }
                    client = client_builder.build().await?;
                    continue;
                }
                Some(ErrorKind::Forbidden) => {
                    println!("Wrong password or username. Trying again.");
                    logged_in = false;
                    continue;
                }
                Some(_) => {
                    return Err(error.into());
                }
            },
        }
    }

    // add our CommandBot to be notified of incoming messages, we do this after the
    // initial sync to avoid responding to messages before the bot was running.
    client.add_event_handler_context(aio.clone());
    if aio.cfg.autojoin {
        client.add_event_handler(on_stripped_state_member);
    }
    client.add_event_handler(on_room_message);

    let client_cc = client.clone();
    tokio::spawn(async move { client.sync(sync_settings).await });

    Ok(client_cc)
}
