use config::Config;
use matrix_sdk::{
    room::Room,
    ruma::{events::room::message::RoomMessageEventContent, OwnedRoomId},
};
use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
};
use tokio::time::{sleep, Duration};

mod matrix;
use matrix::login_and_sync;

mod mozilla;
use mozilla::MozData;

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

#[derive(Clone)]
pub struct SharedState {
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
    // ------- Getting the login-credentials from file ------
    // You can get them however you like: hard-code them here, env-variabl,
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
        MozData::new("firefox/candidates", Some("esr"), true),
        MozData::new("firefox/releases", Some("esr"), false),
        MozData::new("thunderbird/candidates", Some("candidates"), true),
        MozData::new("thunderbird/releases", None, false),
        MozData::new("security/nss/releases", None, false),
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
