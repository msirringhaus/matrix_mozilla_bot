use super::{LoginData, SharedState};
use matrix_sdk::{
    config::SyncSettings,
    event_handler::Ctx,
    room::Room,
    ruma::{
        events::room::member::StrippedRoomMemberEvent,
        events::room::message::{
            MessageType, OriginalSyncRoomMessageEvent, RoomMessageEventContent,
            TextMessageEventContent,
        },
        ServerName, UserId,
    },
    Client, Session,
};
use tokio::time::{sleep, Duration};

async fn on_room_message(
    event: OriginalSyncRoomMessageEvent,
    room: Room,
    client: Client,
    ctx: Ctx<SharedState>,
) -> anyhow::Result<()> {
    if let Room::Joined(room) = room {
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
                    room.send(content, None).await?;
                }
                if body == "!leave" {
                    let content = RoomMessageEventContent::text_plain("Bye");
                    room.send(content, None).await?;
                    room.leave().await?;
                }
                if body == "!watch" {
                    let content = RoomMessageEventContent::text_plain("Watching...");
                    room.send(content, None).await?;
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

    if let Room::Invited(room) = room {
        tokio::spawn(async move {
            if ctx.cfg.accept_commands_from.is_empty()
                || ctx.cfg.accept_commands_from.contains(&room_member.sender)
            {
                println!("Autojoining room {}", room.room_id());
                let mut delay = 2;

                while let Err(err) = room.accept_invitation().await {
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
                while let Err(err) = room.reject_invitation().await {
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

pub async fn login_and_sync(aio: SharedState) -> anyhow::Result<Client> {
    #[allow(unused_mut)]
    let mut client_builder = Client::builder().homeserver_url(aio.cfg.homeserver_url.clone());

    let client = client_builder.build().await?;
    match &aio.cfg.login_data {
        LoginData::UsernamePassword(username, password) => {
            client
                .login_username(username, password)
                .initial_device_display_name("Mozilla bot")
                .send()
                .await?;
            println!("logged in as {}", username);
        }
        LoginData::Session(username, token) => {
            let session = Session {
                access_token: token.to_owned(),
                refresh_token: None,
                user_id: UserId::parse_with_server_name(
                    username.clone(),
                    &ServerName::parse(
                        client
                            .homeserver()
                            .await
                            .host_str()
                            .expect("Homeserver-URL has no host"),
                    )?,
                )?,
                device_id: "MOWAUSSCTB".into(),
            };
            client.restore_login(session).await?;
            println!("logged in with token");
        }
        #[cfg(feature = "sso-login")]
        LoginData::Sso => {
            let response = client
                .login_sso(|sso_url| async move {
                    // Open sso_url
                    println!("{sso_url}");
                    Ok(())
                })
                .initial_device_display_name("My app")
                .send()
                .await
                .unwrap();

            println!(
                "Logged in as {}, got device_id {} and access_token {}",
                response.user_id, response.device_id, response.access_token
            );
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
