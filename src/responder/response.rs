use penumbra_crypto::{Address, Value};
use serenity::{client::Cache, model::id::GuildId, prelude::Mentionable};

#[derive(Debug)]
pub struct Response {
    pub(super) succeeded: Vec<(Address, Vec<Value>)>,
    pub(super) failed: Vec<(Address, String)>,
    pub(super) unparsed: Vec<String>,
    pub(super) remaining: Vec<Address>,
}

impl Response {
    pub async fn summary(&self, cache: impl AsRef<Cache>, guild_id: GuildId) -> String {
        /// Construct a mention for the admin roles for this server
        async fn mention_admins(cache: impl AsRef<Cache>, guild_id: GuildId) -> String {
            cache
                .as_ref()
                .guild_roles(guild_id)
                .await
                .iter()
                .map(IntoIterator::into_iter)
                .flatten()
                .filter(|(_, r)| r.permissions.administrator())
                .map(|(&id, _)| id)
                .map(|role_id| role_id.mention().to_string())
                .collect::<Vec<String>>()
                .join(" ")
        }

        let mut response = String::new();

        if !self.succeeded.is_empty() {
            response.push_str("Successfully sent tokens to the following addresses:");
            for (addr, _values) in self.succeeded.iter() {
                response.push_str(&format!("\n`{}`", addr));
            }
        }

        if !self.failed.is_empty() {
            response.push_str("Failed to send tokens to the following addresses:");
            for (addr, error) in self.failed.iter() {
                response.push_str(&format!("\n`{}` (error: {})", addr, error));
            }

            response.push_str(&format!(
                "\n{mention_admins}: you may want to investigate this error :)",
                mention_admins = mention_admins(cache, guild_id).await,
            ))
        }

        if !self.unparsed.is_empty() {
            response.push_str(
                "\nThe following _look like_ Penumbra addresses, \
                but are invalid (maybe a typo or old address version?):",
            );
            for addr in self.unparsed.iter() {
                response.push_str(&format!("\n`{}`", addr));
            }
        }

        if !self.remaining.is_empty() {
            response.push_str(&format!(
                "\nI'm only allowed to send tokens to addresses {} at a time; \
                try again later to get tokens for the following addresses:",
                self.succeeded.len(),
            ));
            for addr in self.remaining.iter() {
                response.push_str(&format!("\n`{}`", addr));
            }
        }

        response
    }
}
