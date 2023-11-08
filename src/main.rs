use config::{Config, ConfigError, Value};
use matrix_sdk::{
    ruma::{events::room::message::RoomMessageEventContent, OwnedRoomId, OwnedUserId, UserId},
    RoomState,
};
use regex::Regex;
use secret_service::{blocking, EncryptionType};
use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tokio::time::{sleep, Duration};

mod matrix;
use matrix::login_and_sync;

mod mozilla;
use mozilla::MozData;

#[allow(unused)]
#[derive(Debug, Clone)]
enum LoginData {
    UsernamePassword(String, String),
    #[cfg(feature = "sso-login")]
    Sso,
}

#[derive(Debug, Clone)]
pub struct SessionDB {
    db_path: PathBuf, // TODO: Make this an enum and add more storage-backends
    db_pw: String,
}

#[derive(Debug, Clone)]
pub struct PlainSessionStorage {
    session_path: PathBuf,
}

#[derive(Debug, Clone)]
pub enum SessionStorage {
    Ephemeral,
    Plain(SessionDB, PlainSessionStorage),
    SecretService(SessionDB),
}

impl SessionStorage {
    fn session_store_exists(&self) -> bool {
        match self {
            SessionStorage::Ephemeral => false,
            SessionStorage::Plain(db, session) => {
                db.db_path.exists() && session.session_path.exists()
            }
            SessionStorage::SecretService(db) => {
                db.db_path.exists() && blocking::SecretService::connect(EncryptionType::Dh).is_ok()
            }
        }
    }

    fn get_session_db(&self) -> Option<SessionDB> {
        match self {
            SessionStorage::Ephemeral => None,
            SessionStorage::Plain(db, _) | SessionStorage::SecretService(db) => Some(db.clone()),
        }
    }
}

#[derive(Debug, Clone)]
struct BotConfig {
    login_data: LoginData,
    homeserver_url: String,
    session_storage: SessionStorage,
    ignore_own_messages: bool,
    autojoin: bool,
    accept_commands_from: Vec<OwnedUserId>,
}

impl BotConfig {
    fn new(
        login_data: LoginData,
        homeserver_url: String,
        session_storage: SessionStorage,
        ignore_own_messages: bool,
        autojoin: bool,
        accept_commands_from: Vec<OwnedUserId>,
    ) -> Self {
        Self {
            login_data,
            homeserver_url,
            session_storage,
            ignore_own_messages,
            autojoin,
            accept_commands_from,
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

fn extract_session_storage(settings: &Config) -> anyhow::Result<SessionStorage> {
    if !settings.get_bool("login.persist_session").unwrap_or(true) {
        return Ok(SessionStorage::Ephemeral);
    }

    let db_path = if let Ok(db_storage) = settings.get_string("login.db_path") {
        PathBuf::from(db_storage)
    } else {
        dirs::data_dir()
            .unwrap_or(PathBuf::from("./"))
            .join("matrix_mozilla_bot")
            .join("session")
    };
    let db_pw = if let Ok(db_pw) = settings.get_string("login.db_pw") {
        db_pw
    } else {
        rpassword::prompt_password_stderr(&format!(
            "Enter Session storage ({}) password: ",
            db_path.to_string_lossy()
        ))?
    };
    if !settings
        .get_bool("login.use_secret_service")
        .unwrap_or(true)
    {
        let session_path = if let Ok(session_path) = settings.get_string("login.session_path") {
            PathBuf::from(session_path)
        } else {
            db_path.join("session.dump")
        };
        Ok(SessionStorage::Plain(
            SessionDB { db_path, db_pw },
            PlainSessionStorage { session_path },
        ))
    } else {
        Ok(SessionStorage::SecretService(SessionDB { db_path, db_pw }))
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
    let session_storage = extract_session_storage(&settings)?;
    #[cfg(feature = "sso-login")]
    let login_data = LoginData::Sso;
    #[cfg(not(feature = "sso-login"))]
    let login_data = {
        let username = settings.get_string("login.username")?;
        let password = match settings.get_string("login.password") {
            Ok(pw) => pw,
            Err(..) => {
                // We don't need a login-password, if we can restore the session from disk
                if session_storage.session_store_exists() {
                    String::new()
                } else {
                    rpassword::prompt_password_stderr("Enter Password: ")
                        .expect("Failed to read password")
                }
            }
        };
        LoginData::UsernamePassword(username, password)
    };
    // Currently not really used, but I leave it here in case we need it at some point
    let ignore_own_messages = settings
        .get_bool("config.ignore_own_messages")
        .unwrap_or(true);
    let autojoin = settings.get_bool("config.autojoin").unwrap_or(true);
    let sleep_time_in_minutes = settings
        .get_int("config.sleep_time_in_minutes")
        .unwrap_or(60) as u64;
    let accept_commands_from_str: Vec<String> = settings
        .get_array("config.accept_commands_from")
        .unwrap_or_default()
        .into_iter()
        .map(|x| x.into_string())
        .collect::<Result<Vec<_>, _>>()?;
    let accept_commands_from = accept_commands_from_str
        .into_iter()
        .map(UserId::parse)
        .collect::<Result<Vec<_>, _>>()?;

    let mut sources = Vec::new();
    for (_name, val) in settings.get_table("subscription")? {
        let sub = val.into_table()?;
        let url_part = sub
            .get("url_part")
            .ok_or(ConfigError::NotFound(String::from("url_part")))?
            .clone()
            .into_string()?;
        let query_subdirs = sub
            .get("query_subdirs")
            .ok_or(ConfigError::NotFound(String::from("query_subdirs")))?
            .clone()
            .into_bool()?;
        let filter = sub
            .get("filter")
            .map(Clone::clone)
            .map(Value::into_string)
            .transpose()?
            .map(|x| Regex::new(&x))
            .transpose()?;
        sources.push(MozData::new(&url_part, filter, query_subdirs));
    }
    // -------------------------------------------------------
    let botconfig = BotConfig::new(
        login_data,
        homeserver_url,
        session_storage,
        ignore_own_messages,
        autojoin,
        accept_commands_from,
    );
    let shared_state = SharedState::new(botconfig);

    let client = login_and_sync(shared_state.clone()).await?;

    loop {
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
                    if let Some(room) = client.get_room(&roomid) {
                        if room.state() != RoomState::Joined {
                            continue;
                        }
                        let content = RoomMessageEventContent::text_html(
                            &format!("{} got new uploads: {}", source.url_part, answer_str),
                            &format!(
                                "<a href=\"{}/{}/\">{}</a> got new uploads: {}",
                                source.base_url, source.url_part, source.url_part, answer_str
                            ),
                        );
                        room.send(content).await?;
                    }
                }
            }
        }
        sleep(Duration::from_secs(sleep_time_in_minutes * 60)).await;
    }
}
