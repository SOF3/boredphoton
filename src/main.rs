use std::collections::HashMap;
use std::convert::TryFrom;
use std::fs;
use std::io;
use std::path::Path;

use serde::Deserialize;
use serenity::client::Context;
use serenity::model::*;

mod joins;
use joins::*;
use std::future::Future;

type Result<T, E = Box<dyn std::error::Error>> = std::result::Result<T, E>;

#[tokio::main]
async fn main() -> Result<()> {
    pretty_env_logger::init();

    let config = load_config()?;
    let token = config.discord.token.to_owned();
    let handler = Handler::try_from(config)?;
    let mut client = serenity::Client::builder(token)
        .event_handler(handler)
        .await
        .expect("Error connecting to discord");
    client.start().await.map_err(Into::into)
}

fn load_config() -> Result<Config, config::ConfigError> {
    let mut config = config::Config::new();
    config.merge(config::File::with_name("config"))?;
    config.try_into()
}

#[derive(Deserialize)]
struct Config {
    discord: DiscordConfig,
    channels: HashMap<u64, u64>,
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
    channels: HashMap<u64, u64>,
}

impl TryFrom<Config> for Handler {
    type Error = io::Error;

    fn try_from(config: Config) -> io::Result<Self> {
        let Config {
            discord: DiscordConfig { client_id, .. },
            channels,
        } = config;

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
            channels,
        })
    }
}

#[allow(clippy::unreadable_literal)]
const ADMINS: &[u64] = &[390090409159950338];

#[async_trait::async_trait]
impl serenity::client::EventHandler for Handler {
    async fn guild_member_addition(&self, ctx: Context, guild_id: id::GuildId, _member: guild::Member) {
        trying(|| async {
            let guild = guild::Guild::get(&ctx, guild_id).await?;

            let stat = self.guild_joins.add(guild_id, 1)?;

            log::info!("Guild {} stats: {:?}", &guild.name, &stat,);

            if stat.is_abnormal() {
                if let Some(&channel) = self.channels.get(&guild_id.as_u64()) {
                    let channel = id::ChannelId::from(channel);
                    channel.send_message(&ctx, |m| {
                        m.content(format!(
                            "@here ALERT: abnormal server joins detected, stats = {}",
                            &stat
                        ))
                    }).await?;
                }
            }

            Ok(())
        }).await
    }

    async fn message(&self, ctx: Context, message: channel::Message) {
        trying(|| async {
            let guild = message.guild(&ctx).await;
            let channel = message.channel(&ctx).await;
            if let (Some(guild), Some(channel::Channel::Guild(channel))) = (guild, channel) {
                log::debug!(
                    "Message from {} #{}: <{}> {}",
                    &guild.name,
                    &channel.name,
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
                        message.reply(&ctx, format!("Invite link: {}", &self.invite_link)).await?;
                    }
                    "stat" => {
                        if let Some(guild) = message.guild_id {
                            let stat = self.guild_joins.add(guild, 0)?;
                            message.reply(&ctx, format!("Stats:\n{}", stat)).await?;
                        }
                    }
                    "adm" => {
                        if !ADMINS.contains(message.author.id.as_u64()) {
                            return Ok(());
                        }
                        match args.next() {
                            Some("save") => {
                                self.guild_joins.save()?;
                            }
                            Some("stop") => {
                                self.guild_joins.save()?;
                                std::process::exit(0);
                            }
                            _ => (),
                        }
                    }
                    _ => (),
                }
            }

            Ok(())
        }).await;
    }
}

async fn trying<F,R>(f: F) where F: FnOnce() -> R, R: Future<Output = Result<()>> {
    match f().await {
        Ok(()) => (),
        Err(err) => {
            log::error!("Error handling event: {}", err);
        }
    }
}
