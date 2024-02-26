use std::fmt::Write;

use anyhow::anyhow;
use chrono::{DateTime, Duration, Utc};
use poise::{serenity_prelude::*, CreateReply};
use serenity::{
    futures::{future, StreamExt, TryStreamExt},
    Error as SerenityError,
};
use shuttle_secrets::SecretStore;
use thiserror::Error;
use tokio::time::Instant;
use tracing::error;

use diesel_async::{pooled_connection::deadpool::Pool, AsyncPgConnection, Error};

const METER_LIMIT: usize = 500;

#[derive(Clone)]
struct Data {
    pool: Pool<AsyncPgConnection>,
}

#[derive(Error, Debug)]
enum SlimeError {
    #[error("an error occurred within Serenity: {0}")]
    SerenityError(#[from] SerenityError),
    #[error("an error occurred within sqlx: {0}")]
    DatabaseError(#[from] SqlxError),
}
type Context<'a> = poise::Context<'a, Data, SlimeError>;

fn make_uuid_buttons(yes_uuid: &str, no_uuid: &str, disabled: bool) -> CreateActionRow {
    CreateActionRow::Buttons(vec![
        CreateButton::new(yes_uuid)
            .label("yes")
            .style(ButtonStyle::Danger)
            .disabled(disabled),
        CreateButton::new(no_uuid)
            .label("no")
            .style(ButtonStyle::Secondary)
            .disabled(disabled),
    ])
}

async fn messages_before(
    ctx: Context<'_>,
    before: DateTime<Utc>,
    channel: ChannelId,
) -> Result<Vec<Message>, SlimeError> {
    Ok(channel
        .messages_iter(ctx)
        .skip_while(|v| {
            future::ready(
                v.as_ref()
                    .map(|msg| msg.timestamp.to_utc() >= before)
                    .unwrap_or(false),
            )
        })
        .try_collect()
        .await?)
}

async fn bulk_delete(
    ctx: Context<'_>,
    messages: &[Message],
    dry_run: bool,
) -> Result<(), SlimeError> {
    debug_assert!(
        messages[messages.len() - 1].timestamp.to_utc() > Utc::now() - Duration::weeks(2)
    );

    let mut start_time = Instant::now();

    let mut count = 0;
    for chunk in messages.chunks(100) {
        if !dry_run {
            ctx.channel_id().delete_messages(ctx, chunk).await?;
        }
        count += 1;

        if count >= METER_LIMIT {
            tokio::time::sleep_until(start_time + tokio::time::Duration::from_secs(60)).await;
            count = 0;
            start_time = Instant::now();
        }
    }

    Ok(())
}

async fn slow_bulk_delete(
    ctx: Context<'_>,
    messages: &[Message],
    dry_run: bool,
) -> Result<(), SlimeError> {
    let mut count = 0;
    let mut start_time = Instant::now();

    for message in messages {
        if !dry_run {
            ctx.channel_id().delete_message(ctx, message).await?;
        }
        count += 1;

        if count >= METER_LIMIT {
            tokio::time::sleep_until(start_time + tokio::time::Duration::from_secs(60)).await;
            count = 0;
            start_time = Instant::now();
        }
    }

    Ok(())
}

/// Bulk deletes messages from the supplied channel. Warning: This can take a very long time.
#[poise::command(
    slash_command,
    category = "delete",
    guild_only = true,
    default_member_permissions = "ADMINISTRATOR"
)]
async fn purge_old(
    ctx: Context<'_>,
    #[description = "the channel to purge from"] channel: Channel,
    #[description = "whether to actually run the command or merely show progress as if it were running"]
    dry_run: Option<bool>,
) -> Result<(), SlimeError> {
    let before = Utc::now() - chrono::Duration::days(7);
    let dry_run = dry_run.unwrap_or(false) || cfg!(debug);

    let conn = ctx.data().pool.acquire().await?;

    ctx.defer().await?;

    let messages = messages_before(ctx, before, channel.id()).await?;

    let bulk_cutoff = Utc::now() - (chrono::Duration::days(13) + chrono::Duration::hours(12));

    let mut content = String::from("I'll help you purge old messages!\n\n");
    if messages.is_empty() {
        return Ok(());
    }

    let (slow_index, mut minutes) = if let Some((idx, msg)) = messages
        .iter()
        .enumerate()
        .find(|(_, msg)| msg.timestamp.to_utc() < bulk_cutoff)
    {
        let old_message_count = messages.len() - idx;
        let minutes_to_delete = (old_message_count as f64) / (METER_LIMIT as f64);

        write!(
            &mut content,
            "This deletion has {old_message_count} messages beyond the bulk cutoff window!\n\
            At a rate of {METER_LIMIT} messages per minute, deleting these will take approximately {minutes_to_delete:.2} minutes.\n\
            The first message in this set is <{}>, and the last is <{}>.\n\n",
            messages[messages.len()-1].link(),
            msg.link(),
        )
        .unwrap();

        (Some(idx), minutes_to_delete)
    } else {
        (None, 0.)
    };

    let bulk_count = slow_index.unwrap_or(0);
    minutes += if bulk_count > 0 {
        let msgs_per_min = METER_LIMIT * 100;
        let minutes_to_delete = (bulk_count as f64) / (msgs_per_min as f64);
        write!(
            &mut content,
            "This deletion has {bulk_count} messages that can be *bulk* deleted!\n\
            At a rate of {msgs_per_min} messages per minute, deleting these will take approximately {minutes_to_delete:.2} minutes.\n\
            The first message in this set is <{}>, and the last is <{}>.\n\n",
            messages[bulk_count-1].link(), messages[0].link(),
        ).unwrap();

        minutes_to_delete
    } else {
        0.
    };

    write!(&mut content, "Overall, this will take {minutes:.2} minutes to complete, starting with the bulk messages. Continue?").unwrap();

    let id = ctx.id();
    let yes_uuid: String = format!("{id}-yes");
    let no_uuid: String = format!("{id}-no");

    let buttons = make_uuid_buttons(&yes_uuid, &no_uuid, false);

    let reply = CreateReply::default()
        .content(content)
        .components(vec![buttons]);
    ctx.send(reply).await?;

    if let Some(interactions) = ComponentInteractionCollector::new(ctx.serenity_context())
        .timeout(std::time::Duration::from_secs(120))
        .custom_ids(vec![yes_uuid.clone(), no_uuid.clone()])
        .await
    {
        let message = CreateInteractionResponseMessage::new()
            .components(vec![make_uuid_buttons("yes_disabled", "no_disabled", true)])
            .content(&interactions.message.content);

        let disable_buttons = CreateInteractionResponse::UpdateMessage(message);
        interactions
            .create_response(ctx, disable_buttons)
            .await
            .inspect_err(|e| error!("{}", e))?;

        let content = match &interactions.data.custom_id {
            id if id == &yes_uuid => "yes",
            id if id == &no_uuid => "no",
            _ => unreachable!(),
        };

        let followup = CreateInteractionResponseFollowup::new()
            .content(content)
            .ephemeral(true);
        interactions
            .create_followup(ctx, followup)
            .await
            .inspect_err(|e| error!("{}", e))?;
    }

    Ok(())
}

/// Sets the channel where bot spam (e.g. status updates) should happen. Default: current channel
#[poise::command(
    slash_command,
    category = "delete",
    guild_only = true,
    default_member_permissions = "ADMINISTRATOR"
)]
async fn admin_bot_spam_channel(
    ctx: Context<'_>,
    #[description = "the channel to purge from"] channel: Option<Channel>,
) -> Result<(), SlimeError> {
    let channel = channel.map(|v| v.id()).unwrap_or(ctx.channel_id());

    let mut conn = ctx.data().pool.acquire().await?;

    sqlx::query!(
        "INSERT INTO bot_spam_channels (guild_id, channel_id) VALUES ({}, {});",
        ctx.guild_id().unwrap().to_string(),
        channel.to_string()
    )
    .execute(&mut *conn)
    .await?;

    todo!()
}

#[shuttle_runtime::main]
async fn serenity(
    #[shuttle_secrets::Secrets] secret_store: SecretStore,
    #[shuttle_diesel_async::Postgres] pool: Pool<AsyncPgConnection>,
) -> shuttle_serenity::ShuttleSerenity {
    // Get the discord token set in `Secrets.toml`
    let token = if let Some(token) = secret_store.get("DISCORD_TOKEN") {
        token
    } else {
        return Err(anyhow!("'DISCORD_TOKEN' was not found").into());
    };

    // Set gateway intents, which decides what events the bot will be notified about
    let intents = GatewayIntents::GUILD_MESSAGES
        | GatewayIntents::MESSAGE_CONTENT
        | GatewayIntents::GUILD_SCHEDULED_EVENTS
        | GatewayIntents::DIRECT_MESSAGES;

    let framework = poise::Framework::builder()
        .options(poise::FrameworkOptions {
            commands: vec![purge_old(), admin_bot_spam_channel()],
            ..Default::default()
        })
        .setup(|ctx, _ready, framework| {
            Box::pin(async move {
                poise::builtins::register_globally(ctx, &framework.options().commands).await?;
                Ok(Data { pool })
            })
        })
        .build();

    let client = Client::builder(&token, intents)
        .framework(framework)
        .await
        .expect("Err creating client");

    Ok(client.into())
}
