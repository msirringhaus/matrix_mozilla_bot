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
enum LoginData {
    UsernamePassword(String, String),
    Session(String, String),
}

#[derive(Debug, Clone)]
struct BotConfig {
    login_data: LoginData,
    homeserver_url: String,
    ignore_own_messages: bool,
    autojoin: bool,
}

impl BotConfig {
    fn new(
        login_data: LoginData,
        homeserver_url: String,
        ignore_own_messages: bool,
        autojoin: bool,
    ) -> Self {
        Self {
            login_data,
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

    let homeserver_url = settings.get_string("login.homeserver_url")?;
    let use_session = settings.get_bool("login.use_session").unwrap_or(false);
    let username = settings.get_string("login.username")?;
    let password = match settings.get_string("login.password") {
        Ok(pw) => pw,
        Err(..) => {
            rpassword::prompt_password_stderr("Enter Password: ").expect("Failed to read password")
        }
    };
    // If use_session is true, the "password" is really the session-token
    let login_data = if use_session {
        LoginData::Session(username, password)
    } else {
        LoginData::UsernamePassword(username, password)
    };
    // // Currently not really used, but I leave it here in case we need it at some point
    let ignore_own_messages = settings
        .get_bool("config.ignore_own_messages")
        .unwrap_or(true);
    let autojoin = settings.get_bool("config.autojoin").unwrap_or(true);
    let sleep_time_in_minutes = settings
        .get_int("config.sleep_time_in_minutes")
        .unwrap_or(60) as u64;
    // -------------------------------------------------------
    let botconfig = BotConfig::new(login_data, homeserver_url, ignore_own_messages, autojoin);
    let shared_state = SharedState::new(botconfig);
    let mut sources = [
        MozData::new("firefox/candidates", Some("esr"), true),
        MozData::new("firefox/releases", Some("esr"), false),
        MozData::new("thunderbird/candidates", Some("candidates"), true),
        MozData::new("thunderbird/releases", None, false),
        MozData::new("security/nss/releases", None, false),
    ];

    let client = login_and_sync(shared_state.clone()).await?;

    loop {
        sleep(Duration::from_secs(30)).await;
        for source in &mut sources {
            let answer = source.fetch_upstream_and_compare().await?;
            if !answer.is_empty() {
                let mut formatted_answer: Vec<_> = answer.iter().map(|x| x.to_string()).collect();
                formatted_answer.sort();
                let answer_str = formatted_answer.join(", ");
                println!("{} differ: {:?}", source.url_part, answer_str);
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
                            &format!("{} got new uploads: {}", source.url_part, answer_str),
                            &format!(
                                "<a href=\"{}/{}/\">{}</a> got new uploads: {}",
                                source.base_url, source.url_part, source.url_part, answer_str
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
