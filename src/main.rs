use std::collections::{HashMap, VecDeque};
use std::convert::TryFrom;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::RwLock;
use std::time::UNIX_EPOCH;

use crossbeam::sync::ShardedLock;
use serde::{Deserialize, Serialize};
use serenity::client::Context;
use serenity::model::*;

type Result<T, E = Box<dyn std::error::Error>> = std::result::Result<T, E>;

fn main() -> Result<()> {
    pretty_env_logger::init();

    let config = load_config()?;
    let token = config.discord.token.to_owned();
    let handler = Handler::try_from(config)?;

    let mut client = serenity::Client::new(token, handler)?;
    client.start().map_err(Into::into)
}

fn load_config() -> Result<Config, config::ConfigError> {
    let mut config = config::Config::new();
    config.merge(config::File::with_name("config"))?;
    config.try_into()
}

#[derive(Deserialize)]
struct Config {
    discord: DiscordConfig,
}

#[derive(Deserialize)]
struct DiscordConfig {
    client_id: u64,
    token: String,
}

struct Handler {
    mention_matches: Vec<String>,
    invite_link: String,
    guild_joins: GuildJoinsMap,
}

impl TryFrom<Config> for Handler {
    type Error = io::Error;

    fn try_from(config: Config) -> io::Result<Self> {
        let client_id = config.discord.client_id;

        let data_dir = Path::new("data");
        if !data_dir.exists() {
            fs::create_dir_all(&data_dir)?;
        }

        Ok(Self {
            mention_matches: vec![format!("<@!{}> ", client_id), format!("<@{}> ", client_id)],
            invite_link: format!(
                "https://discord.com/oauth2/authorize?client_id={}&scope=bot",
                client_id
            ),
            guild_joins: GuildJoinsMap::new(data_dir.into()),
        })
    }
}

#[allow(clippy::unreadable_literal)]
const ADMINS: &[u64] = &[390090409159950338];

impl serenity::client::EventHandler for Handler {
    fn message(&self, ctx: Context, message: channel::Message) {
        trying(|| {
            let guild = message.guild(&ctx);
            let channel = message.channel(&ctx);
            if let (Some(guild), Some(channel::Channel::Guild(channel))) = (guild, channel) {
                log::debug!(
                    "Message from {} #{}: <{}> {}",
                    &guild.read().name,
                    &channel.read().name,
                    &message.author.name,
                    &message.content
                );
            }

            if self
                .mention_matches
                .iter()
                .any(|pat| message.content.starts_with(pat))
            {
                let content = &message.content[(message
                    .content
                    .find("> ")
                    .expect("checked in mention_matches")
                    + 2)..];
                let mut args = content.split(' ');
                let cmd = args.next().expect("split is nonempty");
                match cmd {
                    "invite" => {
                        message.reply(&ctx, format!("Invite link: {}", &self.invite_link))?;
                    }
                    "adm" => {
                        if !ADMINS.contains(message.author.id.as_u64()) {
                            return Ok(());
                        }
                        #[allow(clippy::single_match)]
                        match args.next() {
                            Some("stop") => {
                                self.guild_joins.finalize();
                                std::process::exit(0);
                            }
                            _ => (),
                        }
                    }
                    _ => (),
                }
            }

            Ok(())
        });
    }

    fn guild_member_addition(&self, ctx: Context, guild_id: id::GuildId, _member: guild::Member) {
        trying(|| {
            let guild = guild::Guild::get(&ctx, guild_id)?;

            let AddResult { current, average } = self.guild_joins.add(guild_id);

            log::info!(
                "Guild {} stats: {} / {} = {}",
                &guild.name,
                current,
                average,
                (current as f64) / average
            );

            Ok(())
        })
    }
}

const JOINS_BACKLOG_SIZE: usize = 720;

fn current_hour() -> u64 {
    UNIX_EPOCH
        .elapsed()
        .expect("System clock is earlire than unix epoch")
        .as_secs()
        / 3600
}

struct GuildJoinsMap {
    lock: ShardedLock<HashMap<id::GuildId, RwLock<GuildJoins>>>,
    data_dir: PathBuf,
}

impl GuildJoinsMap {
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            lock: ShardedLock::default(),
            data_dir,
        }
    }

    pub fn finalize(&self) {
        let lock = self.lock.write().unwrap();

        for (id, gj) in lock.iter() {
            let gj = gj.write().unwrap();
            if let Err(err) = gj.save(self.data_dir.join(format!("{}.json", id))) {
                log::error!("Error saving guild joins: {}", err);
            }
        }
    }

    pub fn add(&self, guild: id::GuildId) -> AddResult {
        loop {
            {
                let guard = self.lock.read().unwrap();
                if let Some(gj) = guard.get(&guild) {
                    {
                        let gjg = gj.read().unwrap();
                        if let Ok(current) = gjg.add() {
                            let average = gjg.average();
                            return AddResult { current, average };
                        }
                    }

                    {
                        let mut gjg = gj.write().unwrap();
                        let current = gjg.add_mut();
                        let average = gjg.average();
                        return AddResult { current, average };
                    }
                }
            }

            {
                let mut guard = self.lock.write().unwrap();
                let gj = GuildJoins::new(self.data_dir.join(format!("{}.json", guild)));
                let _ = guard.insert(guild, RwLock::new(gj));
            }
        }
    }
}

struct AddResult {
    current: u32,
    average: f64,
}

#[derive(Serialize, Deserialize)]
struct GuildJoins {
    current_hour: u64,
    log: VecDeque<Option<u32>>,
    current: AtomicU32,
}

impl GuildJoins {
    fn new(path: impl AsRef<Path>) -> Self {
        Self::load(path).unwrap_or_else(|| Self {
            current_hour: current_hour(),
            log: std::iter::repeat(None).take(JOINS_BACKLOG_SIZE).collect(),
            current: AtomicU32::new(0),
        })
    }

    fn load(path: impl AsRef<Path>) -> Option<Self> {
        serde_json::from_reader(fs::File::open(path).ok()?).ok()
    }

    fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        serde_json::to_writer(fs::File::create(path)?, self)?;
        Ok(())
    }

    fn add(&self) -> Result<u32, ()> {
        let hour = current_hour();
        if hour > self.current_hour {
            return Err(());
        }
        Ok(self.current.fetch_add(1, Ordering::Relaxed))
    }

    fn add_mut(&mut self) -> u32 {
        let hour = current_hour();
        if hour > self.current_hour {
            let diff = (hour - self.current_hour) as usize;
            if diff >= JOINS_BACKLOG_SIZE {
                // reinitialize to all Nones
                self.log = std::iter::repeat(None).take(JOINS_BACKLOG_SIZE).collect();
            } else {
                drop(self.log.drain(..diff));
                self.log.extend(std::iter::repeat(None).take(diff));
            }
            let back = self.log.back_mut().expect("JOINS_BACKLOG_SIZE > 0");
            *back = Some(*self.current.get_mut());
        }

        debug_assert_eq!(self.log.len(), JOINS_BACKLOG_SIZE);

        let current = self.current.get_mut();
        *current += 1;
        *current
    }

    fn average(&self) -> f64 {
        let mut count = 0;
        let mut sum = 0;
        for option in &self.log {
            if let Some(joins) = option {
                count += 1;
                sum += joins;
            }
        }

        if count == 0 {
            f64::NAN
        } else {
            (sum as f64) / (count as f64)
        }
    }
}

fn trying(f: impl FnOnce() -> Result<()>) {
    match f() {
        Ok(()) => (),
        Err(err) => {
            log::error!("Error handling event: {}", err);
        }
    }
}
