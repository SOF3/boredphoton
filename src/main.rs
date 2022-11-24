use std::collections::HashMap;
use std::convert::TryFrom;
use std::fs;
use std::io;
use std::path::Path;

use serde::Deserialize;
use serenity::client::Context;
use serenity::model::prelude::{ChannelId, GuildId, UserId};
use serenity::model::{channel, guild};
use serenity::prelude::GatewayIntents;

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
    let intents = GatewayIntents::non_privileged()
        | GatewayIntents::MESSAGE_CONTENT
        | GatewayIntents::GUILD_MEMBERS;
    let mut client = serenity::Client::builder(token, intents)
        .event_handler(handler)
        .await
        .expect("Error connecting to discord");
    client.start().await.map_err(Into::into)
}

fn load_config() -> Result<Config, config::ConfigError> {
    let config = config::Config::builder()
        .add_source(config::File::with_name("config"))
        .build()?;
    config.try_deserialize()
}

#[derive(Deserialize)]
struct Config {
    admin_ids: Box<[UserId]>,
    discord: DiscordConfig,
    channels: HashMap<GuildId, ChannelId>,
}

#[derive(Deserialize)]
struct DiscordConfig {
    client_id: u64,
    token: String,
}

struct Handler {
    admin_ids: Box<[UserId]>,
    mention_matches: Vec<String>,
    invite_link: String,
    guild_joins: GuildJoinsMap,
    channels: HashMap<GuildId, ChannelId>,
}

impl TryFrom<Config> for Handler {
    type Error = io::Error;

    fn try_from(config: Config) -> io::Result<Self> {
        let Config {
            admin_ids,
            discord: DiscordConfig { client_id, .. },
            channels,
        } = config;

        let data_dir = Path::new("data");
        if !data_dir.exists() {
            fs::create_dir_all(data_dir)?;
        }

        Ok(Self {
            mention_matches: vec![format!("<@!{}> ", client_id), format!("<@{}> ", client_id)],
            invite_link: format!(
                "https://discord.com/oauth2/authorize?client_id={}&scope=bot",
                client_id
            ),
            guild_joins: GuildJoinsMap::new(data_dir.into()),
            channels,
            admin_ids,
        })
    }
}

#[async_trait::async_trait]
impl serenity::client::EventHandler for Handler {
    async fn guild_member_addition(&self, ctx: Context, member: guild::Member) {
        trying(|| async {
            let guild_id = member.guild_id;
            let guild = guild::Guild::get(&ctx, guild_id).await?;

            let stat = self.guild_joins.add(guild_id, 1)?;

            log::info!("Guild {} stats: {:?}", &guild.name, &stat,);

            if stat.is_abnormal() {
                if let Some(&channel) = self.channels.get(&guild_id) {
                    channel
                        .send_message(&ctx, |m| {
                            m.content(format!(
                                "@here ALERT: abnormal server joins detected, stats = {}",
                                &stat
                            ))
                        })
                        .await?;
                }
            }

            Ok(())
        })
        .await
    }

    async fn message(&self, ctx: Context, message: channel::Message) {
        trying(|| async {
            let guild = message.guild(&ctx);
            let channel = message.channel(&ctx).await;
            if let (Some(guild), Ok(channel::Channel::Guild(channel))) = (guild, channel) {
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
                        message
                            .reply(&ctx, format!("Invite link: {}", &self.invite_link))
                            .await?;
                    }
                    "stat" => {
                        if let Some(guild) = message.guild_id {
                            let stat = self.guild_joins.add(guild, 0)?;
                            message.reply(&ctx, format!("Stats:\n{}", stat)).await?;
                        }
                    }
                    "adm" => {
                        if !self.admin_ids.contains(&message.author.id) {
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
        })
        .await;
    }
}

async fn trying<F, R>(f: F)
where
    F: FnOnce() -> R,
    R: Future<Output = Result<()>>,
{
    match f().await {
        Ok(()) => (),
        Err(err) => {
            log::error!("Error handling event: {}", err);
        }
    }
}
