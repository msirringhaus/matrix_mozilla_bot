use config::Config;
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
        OwnedRoomId,
    },
    Client,
};
use scraper::{Html, Selector};
use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
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
        if let MessageType::Text(TextMessageEventContent { body, .. }) = event.content.msgtype {
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
    Ok(())
}

async fn on_stripped_state_member(
    room_member: StrippedRoomMemberEvent,
    client: Client,
    room: Room,
) {
    if room_member.state_key != client.user_id().unwrap() {
        return;
    }

    if let Room::Invited(room) = room {
        tokio::spawn(async move {
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
        });
    }
}

async fn login_and_sync(aio: SharedState) -> anyhow::Result<Client> {
    #[allow(unused_mut)]
    let mut client_builder = Client::builder().homeserver_url(aio.cfg.homeserver_url.clone());

    let client = client_builder.build().await?;
    client
        .login_username(&aio.cfg.username, &aio.cfg.password)
        .initial_device_display_name("Command bot")
        .send()
        .await?;

    println!("logged in as {}", aio.cfg.username);

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

#[derive(Debug, Clone)]
struct BotConfig {
    username: String,
    password: String,
    homeserver_url: String,
    ignore_own_messages: bool,
    autojoin: bool,
}

impl BotConfig {
    fn new(
        username: String,
        password: String,
        homeserver_url: String,
        ignore_own_messages: bool,
        autojoin: bool,
    ) -> Self {
        Self {
            username,
            password,
            homeserver_url,
            ignore_own_messages,
            autojoin,
        }
    }
}

struct Data {
    url_part: String,
    query_subdirs: bool,
    filter: Option<String>,
    data: HashSet<String>,
    base_url: String,
}

impl Data {
    fn new(url_part: &str, filter: Option<&str>, query_subdirs: bool) -> Self {
        Self {
            url_part: url_part.to_string(),
            query_subdirs,
            filter: filter.map(str::to_string),
            data: HashSet::new(),
            base_url: "https://ftp.mozilla.org/pub".to_string(),
        }
    }

    async fn fetch_upstream_and_compare(&mut self) -> anyhow::Result<HashSet<String>> {
        let answer = self.query_url().await?;
        let res = answer.difference(&self.data).map(String::clone).collect();
        self.data = answer;
        Ok(res)
    }

    async fn query_subdir(
        base_url: String,
        url_part: String,
        cand: String,
    ) -> anyhow::Result<HashSet<String>> {
        let html = reqwest::get(&format!("{}/{}/{}/", base_url, url_part, cand))
            .await?
            .text()
            .await?;
        let document = Html::parse_document(&html);
        let selector = Selector::parse("a").unwrap();
        let candidates = document
            .select(&selector)
            .map(|x| x.inner_html())
            .filter(|x| x != "..")
            .map(|x| format!("{}/{}", cand, x))
            .collect();
        Ok(candidates)
    }

    async fn query_url(&self) -> anyhow::Result<HashSet<String>> {
        let url = format!("{}/{}/", self.base_url, self.url_part);
        let html = reqwest::get(&url).await?.text().await?;
        let document = Html::parse_document(&html);
        let selector = Selector::parse("a").unwrap();
        let candidates: HashSet<_> = document
            .select(&selector)
            .map(|x| x.inner_html().trim_end_matches('/').to_string())
            .filter(|x| {
                if let Some(filt) = &self.filter {
                    x.contains(filt)
                } else {
                    true
                }
            })
            .collect();

        let outputs = if self.query_subdirs {
            let mut tasks = Vec::with_capacity(candidates.len());
            for cand in candidates {
                tasks.push(tokio::spawn(Self::query_subdir(
                    self.base_url.clone(),
                    self.url_part.clone(),
                    cand.clone(),
                )));
            }

            let mut outputs = HashSet::new();
            for task in tasks {
                let res = task.await?;
                outputs = outputs.union(&res?).map(String::clone).collect();
            }
            outputs
        } else {
            candidates
        };

        Ok(outputs)
    }
}

#[derive(Clone)]
struct SharedState {
    cfg: BotConfig,
    rooms: Arc<Mutex<HashSet<OwnedRoomId>>>,
}

impl SharedState {
    fn new(cfg: BotConfig) -> Self {
        Self {
            cfg,
            rooms: Arc::new(Mutex::new(HashSet::new())),
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // ------- Getting the login-credentials from file -------
    // You can get them however you like: hard-code them here, env-variable,
    // tcp-connection, read from file, etc. Here, we use the config-crate to
    // load from botconfig.toml.
    // Change this file to your needs, if you want to use this example binary.
    let settings = Config::builder()
        .add_source(config::File::with_name("botconfig"))
        // Add in settings from the environment (with a prefix of BOT)
        // Eg.. `BOT_DEBUG=1 ./target/app` would set the `debug` key
        .add_source(config::Environment::with_prefix("BOT"))
        .build()?;

    let username = settings.get_string("username")?;
    let password = settings.get_string("password")?;
    let homeserver_url = settings.get_string("homeserver_url")?;
    // // Currently not really used, but I leave it here in case we need it at some point
    let ignore_own_messages = settings.get_bool("ignore_own_messages").unwrap_or(true);
    let autojoin = settings.get_bool("autojoin").unwrap_or(true);
    // -------------------------------------------------------
    let botconfig = BotConfig::new(
        username,
        password,
        homeserver_url,
        ignore_own_messages,
        autojoin,
    );
    let shared_state = SharedState::new(botconfig);
    let mut sources = [
        Data::new("firefox/candidates", Some("esr"), true),
        Data::new("firefox/releases", Some("esr"), false),
        Data::new("thunderbird/candidates", Some("candidates"), true),
        Data::new("thunderbird/releases", None, false),
        Data::new("security/nss/releases", None, false),
    ];

    let client = login_and_sync(shared_state.clone()).await?;

    let sleep_time_in_minutes = 30;
    loop {
        sleep(Duration::from_secs(30)).await;
        for source in &mut sources {
            let answer = source.fetch_upstream_and_compare().await?;
            if !answer.is_empty() {
                println!("{} differ: {:?}", source.url_part, answer);
                let roomids: Vec<_> = shared_state
                    .rooms
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|x| x.to_owned())
                    .collect();
                for roomid in roomids {
                    if let Some(Room::Joined(room)) = client.get_room(&roomid) {
                        let content = RoomMessageEventContent::text_html(
                            &format!("{} got new uploads: {:?}", source.url_part, answer),
                            &format!(
                                "<a href=\"{}/{}/\">{}</a> got new uploads: {:?}",
                                source.base_url, source.url_part, source.url_part, answer
                            ),
                        );
                        room.send(content, None).await?;
                    }
                }
            }
        }
        sleep(Duration::from_secs(sleep_time_in_minutes * 60)).await;
    }
}
