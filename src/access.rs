//! Who a turn acts for, and what they may make the agent do.
//!
//! Every turn runs as an [`Actor`] set by the channel that received it: the
//! terminal and the authenticated Web UI act as the owner; QQ and email
//! senders are guests unless `access.owners` names them. Tools check the
//! actor's capabilities when they run, so a model that calls a tool it was
//! not offered is still refused.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// A group of tools that can be granted on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// `shell`, still subject to `tools.shell`.
    Shell,
    /// `read_file` and `list_dir`: anything the Gateway's user can read.
    FilesRead,
    /// `write_file` and `edit_file`, still subject to `tools.write`.
    FilesWrite,
    /// Save, search and delete memories; non-owners only see their own.
    Memory,
    /// Add, list and remove scheduled jobs; non-owners only see their own.
    Cron,
    /// Propose and change the agent's identity, shared by every channel.
    Identity,
    /// `web_search`, which spends the search provider's credits.
    WebSearch,
    /// `browser`: opens any web page, including ones on the local network.
    Browser,
    /// `web_fetch`: reads public web pages.
    WebFetch,
    /// Tools of the configured MCP servers (`<server>__<tool>`), which can
    /// do whatever those servers do.
    Mcp,
    /// `skill`: loads the instructions of installed skills.
    Skills,
    /// `send_mail`: sends email from the bot's address, still subject to
    /// `tools.mail` and `mail.mode`.
    Mail,
    /// `settings`: shows and changes the companion settings (heartbeat,
    /// inner life, QQ reply splitting, language) when asked in chat.
    Settings,
    /// `image_generate`: draws pictures, which spends the image model's
    /// credits or the ChatGPT plan's image quota.
    ImageGenerate,
    /// `voice_send`: speaks messages aloud, which spends the speech
    /// model's credits.
    Voice,
}

impl Capability {
    pub const ALL: [Capability; 15] = [
        Capability::Shell,
        Capability::FilesRead,
        Capability::FilesWrite,
        Capability::Memory,
        Capability::Cron,
        Capability::Identity,
        Capability::WebSearch,
        Capability::Browser,
        Capability::WebFetch,
        Capability::Mcp,
        Capability::Skills,
        Capability::Mail,
        Capability::Settings,
        Capability::ImageGenerate,
        Capability::Voice,
    ];

    /// The capability a built-in tool needs; `None` for tools anyone may call.
    pub fn for_tool(tool: &str) -> Option<Capability> {
        Some(match tool {
            "shell" => Capability::Shell,
            "read_file" | "list_dir" => Capability::FilesRead,
            "write_file" | "edit_file" => Capability::FilesWrite,
            "memory_save" | "memory_search" | "memory_delete" => Capability::Memory,
            "cron_add" | "cron_list" | "cron_remove" => Capability::Cron,
            "identity_set" | "identity_propose" => Capability::Identity,
            "web_search" => Capability::WebSearch,
            "browser" => Capability::Browser,
            "web_fetch" => Capability::WebFetch,
            "skill" => Capability::Skills,
            "send_mail" => Capability::Mail,
            "settings" => Capability::Settings,
            "image_generate" | "drawing_profile_save" | "sticker_save" => Capability::ImageGenerate,
            "voice_send" | "voice_profile_save" => Capability::Voice,
            name if crate::mcp::is_mcp_tool(name) => Capability::Mcp,
            _ => return None,
        })
    }
}

/// The authenticated sender of a turn, as its channel identified them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Actor {
    /// `cli`, `web`, `qq:<openid>`, `feishu:<open_id>`, `zulip:<email>` or
    /// `mail:<address>`.
    pub id: String,
    pub owner: bool,
    pub capabilities: BTreeSet<Capability>,
}

/// Actor id of the local terminal.
pub const CLI: &str = "cli";
/// Actor id of the token-authenticated Web UI.
pub const WEB: &str = "web";

impl Actor {
    /// Someone with the host's own authority: every capability.
    pub fn owner(id: &str) -> Self {
        Self {
            id: id.to_owned(),
            owner: true,
            capabilities: Capability::ALL.into_iter().collect(),
        }
    }

    pub fn can(&self, capability: Capability) -> bool {
        self.capabilities.contains(&capability)
    }

    /// Whether this actor may see or change a record another actor created.
    /// Records from before ownership was tracked belong to the owner.
    pub fn owns(&self, created_by: Option<&str>) -> bool {
        self.owner || created_by == Some(self.id.as_str())
    }

    /// The `created_by` filter for store queries: `None` sees everything.
    pub fn scope(&self) -> Option<&str> {
        (!self.owner).then_some(self.id.as_str())
    }
}

tokio::task_local! {
    static CURRENT_ACTOR: Actor;
}

/// Runs `turn` on behalf of `actor`.
pub async fn with_actor<F: std::future::Future>(actor: Actor, turn: F) -> F::Output {
    CURRENT_ACTOR.scope(actor, turn).await
}

/// The actor of the turn running on this task; `None` outside any turn,
/// which tools treat as having no capabilities at all.
pub fn current() -> Option<Actor> {
    CURRENT_ACTOR.try_with(Clone::clone).ok()
}

/// `[access]`: which channel senders are trusted with what.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AccessConfig {
    /// Senders treated like the terminal: `qq:<openid>`, `feishu:<open_id>`,
    /// `zulip:<email>` or `mail:<address>`.
    pub owners: Vec<String>,
    /// On a chat app (QQ, Feishu, Zulip) with no owner yet, the first
    /// person to write to the bot in a private chat becomes its owner.
    pub first_chat_owner: bool,
    /// What any other sender may do.
    pub guest: Vec<Capability>,
    /// Extra capabilities by sender (`qq:<openid>`, `mail:<address>`), by
    /// session (`qq:group:<group_openid>`, `mail:<address>`), or by channel
    /// (`qq:*`, `mail:*`).
    pub grants: BTreeMap<String, Vec<Capability>>,
}

impl Default for AccessConfig {
    fn default() -> Self {
        Self {
            owners: Vec::new(),
            first_chat_owner: false,
            guest: vec![Capability::WebSearch, Capability::WebFetch],
            grants: BTreeMap::new(),
        }
    }
}

impl AccessConfig {
    /// The actor for sender `id` writing in `session`.
    pub fn resolve(&self, id: &str, session: &str) -> Actor {
        if id == CLI || id == WEB {
            return Actor::owner(id);
        }
        let id_lower = id.to_lowercase();
        if self
            .owners
            .iter()
            .any(|o| o.trim().to_lowercase() == id_lower)
        {
            return Actor::owner(id);
        }
        let channel = id
            .split_once(':')
            .map(|(channel, _)| format!("{channel}:*"));
        let mut capabilities: BTreeSet<Capability> = self.guest.iter().copied().collect();
        for (key, granted) in &self.grants {
            let key = key.trim().to_lowercase();
            if key == id_lower || key == session.to_lowercase() || Some(&key) == channel.as_ref() {
                capabilities.extend(granted.iter().copied());
            }
        }
        Actor {
            id: id.to_owned(),
            owner: false,
            capabilities,
        }
    }

    /// Whether `id`, writing in `session`, becomes an owner by writing
    /// first: `first_chat_owner` is on, `session` is their private chat on a
    /// chat app, and nobody owns that app yet.
    pub fn claimable(&self, id: &str, session: &str) -> bool {
        let Some((channel, _)) = id.split_once(':') else {
            return false;
        };
        self.first_chat_owner
            && crate::chat::private_sender(session).is_some_and(|s| s.eq_ignore_ascii_case(id))
            && self.unowned(channel)
    }

    /// Whether no owner is on `channel` (`qq`, `feishu`, `zulip`, `mail`).
    pub fn unowned(&self, channel: &str) -> bool {
        !self.owners.iter().any(|o| {
            o.trim()
                .split_once(':')
                .is_some_and(|(c, _)| c.eq_ignore_ascii_case(channel))
        })
    }

    /// Capabilities every sender on `channel` gets without being named, which
    /// is what an open allow-list hands to strangers.
    pub fn unnamed(&self, channel: &str) -> BTreeSet<Capability> {
        let mut capabilities: BTreeSet<Capability> = self.guest.iter().copied().collect();
        if let Some(granted) = self.grants.get(&format!("{channel}:*")) {
            capabilities.extend(granted.iter().copied());
        }
        capabilities
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_private_chat_on_an_app_without_owner_claims_it() {
        let mut config: AccessConfig =
            toml::from_str(r#"owners = ["zulip:me@example.org"]"#).unwrap();
        // Off by default.
        assert!(!config.claimable("qq:ALICE", "qq:c2c:ALICE"));
        config.first_chat_owner = true;
        assert!(config.claimable("qq:ALICE", "qq:c2c:ALICE"));
        assert!(config.claimable("feishu:ou_a", "feishu:dm:ou_a"));
        // Not in a group, not in someone else's chat, not by email.
        assert!(!config.claimable("qq:ALICE", "qq:group:G"));
        assert!(!config.claimable("qq:ALICE", "qq:c2c:BOB"));
        assert!(!config.claimable("mail:a@example.org", "mail:a@example.org"));
        // Zulip already has its owner.
        assert!(!config.claimable("zulip:eve@example.org", "zulip:dm:eve@example.org"));
        config.owners.push("QQ:BOB".into());
        assert!(!config.claimable("qq:ALICE", "qq:c2c:ALICE"));
    }

    #[test]
    fn channels_resolve_to_owner_guest_or_granted() {
        let config: AccessConfig = toml::from_str(
            r#"
            owners = ["qq:BOSS", "mail:Me@Example.org"]
            guest = []
            [grants]
            "qq:FRIEND" = ["memory"]
            "qq:group:G1" = ["web_search"]
            "mail:*" = ["cron"]
            "#,
        )
        .unwrap();
        assert!(config.resolve(CLI, "main").owner);
        assert!(config.resolve(WEB, "main").owner);
        assert!(config.resolve("qq:BOSS", "qq:c2c:BOSS").owner);
        assert!(
            config
                .resolve("mail:me@example.org", "mail:me@example.org")
                .owner
        );

        let stranger = config.resolve("qq:X", "qq:c2c:X");
        assert!(!stranger.owner && stranger.capabilities.is_empty());
        let friend = config.resolve("qq:FRIEND", "qq:c2c:FRIEND");
        assert_eq!(friend.capabilities, [Capability::Memory].into());
        // A group grant applies to every member writing in that group only.
        let member = config.resolve("qq:M", "qq:group:G1");
        assert_eq!(member.capabilities, [Capability::WebSearch].into());
        assert!(
            config
                .resolve("qq:M", "qq:group:G2")
                .capabilities
                .is_empty()
        );
        let mailer = config.resolve("mail:a@b.c", "mail:a@b.c");
        assert_eq!(mailer.capabilities, [Capability::Cron].into());
        assert_eq!(config.unnamed("mail"), [Capability::Cron].into());
    }

    #[test]
    fn guests_only_own_their_records() {
        let guest = AccessConfig::default().resolve("qq:X", "qq:c2c:X");
        assert!(guest.can(Capability::WebSearch) && !guest.can(Capability::Shell));
        assert!(guest.owns(Some("qq:X")));
        assert!(!guest.owns(Some("qq:Y")) && !guest.owns(None));
        assert_eq!(guest.scope(), Some("qq:X"));
        let owner = Actor::owner(CLI);
        assert!(owner.owns(None) && owner.owns(Some("qq:Y")));
        assert_eq!(owner.scope(), None);
    }
}
