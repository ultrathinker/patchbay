//! (S15) Helpers: scripts and subagents that connect on behalf of an agent.
//!
//! ## The problem this closes
//! Patchbay identifies a caller by a string the caller picks (the
//! `X-Patchbay-Client` header, else `clientInfo.name`). Real agents have that
//! string in their MCP config and never change it. The scripts those agents
//! WRITE do not: a one-off Python or curl helper has to put something in
//! `clientInfo.name`, nothing tells it what, so it invents a name -- `probe`,
//! `orchestrator`, `bee-put`. Every invented name used to raise a system-modal
//! approval dialog and then stay forever, in the known list on Yes and in the
//! forbidden list on No. One machine collected 34 "agents" this way, about half
//! of them such scripts.
//!
//! It was also a permissions hole. An invented name that got a Yes had no
//! per-agent list of its own, so it fell back to the GLOBAL one -- a script
//! written by an agent that was denied a server could reach that server.
//!
//! ## The rule
//! A caller that belongs to an agent says so in its name: `Owner/label`.
//! When `Owner` is an identity Patchbay already knows, the session is a
//! *helper* of that owner:
//!
//! - no approval is asked and nothing is recorded -- the user approved the
//!   owner, and the helper is only visible while it is connected;
//! - it gets NO servers by default. The owner hands some over deliberately,
//!   with the `X-Patchbay-Jacks` header on `initialize`: a comma-separated list
//!   of server names, or `*` for all of the owner's;
//! - what it can reach is that list INTERSECTED with what the owner can reach
//!   right now, re-evaluated on every request. A helper can never hold more
//!   than its owner, and loses a server the moment its owner does.
//!
//! This is bookkeeping, not authentication: identities here are self-reported
//! (see `docs/security.md`), and a local process could always have claimed the
//! owner's bare name. What changes is that the lazy path and the correct path
//! are now the same path.

use std::collections::BTreeSet;

/// Separates the owner from the helper's label in a client identity.
pub const HELPER_SEPARATOR: char = '/';

/// The pseudo server name that grants a helper the gateway-owned
/// `patchbay__*` meta tools. They are namespaced like a jack's tools, so they
/// are granted like a jack's tools.
pub const META_TOOLS_GRANT: &str = "patchbay";

/// Upper bound on names accepted from one `X-Patchbay-Jacks` header. Far above
/// any real jack count; it only stops a hostile header from growing a session.
const MAX_GRANTED_NAMES: usize = 64;

/// The first sentence of `initialize`'s `instructions`, sent to every caller.
pub const BASE_INSTRUCTIONS: &str = "Patchbay exposes upstream MCP servers as namespaced tools named <jack>__<tool>. If a tool call returns an \"unpatched\" error, the user has disabled that server in the Patchbay tray; ask them to re-enable it there.";

/// Which of its owner's servers a helper asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JackGrant {
    /// `X-Patchbay-Jacks: *` -- everything the owner has, the meta tools too.
    All,
    /// The named servers only. EMPTY is the default and means nothing at all.
    Only(BTreeSet<String>),
}

impl JackGrant {
    /// Parse an `X-Patchbay-Jacks` header value.
    ///
    /// Absent, empty or unparseable means NOTHING, never everything: a helper
    /// whose author forgot the header must end up with no access rather than
    /// with all of its owner's. Names that could not be a jack name are
    /// dropped -- they could not match one anyway, and what is kept is shown
    /// in the window.
    pub fn parse(header: Option<&str>) -> JackGrant {
        let mut names = BTreeSet::new();
        for part in header.unwrap_or("").split(',') {
            let part = part.trim();
            if part == "*" {
                return JackGrant::All;
            }
            if crate::config::is_valid_jack_name(part) && names.len() < MAX_GRANTED_NAMES {
                names.insert(part.to_string());
            }
        }
        JackGrant::Only(names)
    }

    /// Did the helper ask for this server? This is only half of the answer:
    /// the owner must also be able to reach it.
    pub fn allows(&self, jack: &str) -> bool {
        match self {
            JackGrant::All => true,
            JackGrant::Only(names) => names.contains(jack),
        }
    }

    /// Short human text for the window and the log: `*`, `a, b` or `none`.
    pub fn describe(&self) -> String {
        match self {
            JackGrant::All => "*".to_string(),
            JackGrant::Only(names) if names.is_empty() => "none".to_string(),
            JackGrant::Only(names) => names.iter().cloned().collect::<Vec<_>>().join(", "),
        }
    }
}

/// What makes a session a helper: its label and what it was granted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HelperInfo {
    pub label: String,
    pub grant: JackGrant,
}

/// Split `Owner/label` into its two halves, when `Owner` is a known identity.
///
/// Returns `None` for a name with no separator, an empty half, or an owner
/// nobody has heard of -- such a name is an ordinary identity and takes the
/// ordinary path. That last case matters: a real client may well be called
/// `@scope/tool`, and it must not turn into a helper of a `@scope` that does
/// not exist.
///
/// The LONGEST known owner wins, so a helper of an agent whose own name
/// contains a separator still resolves to that agent.
pub fn split_helper<'a>(
    name: &'a str,
    is_known: impl Fn(&str) -> bool,
) -> Option<(&'a str, &'a str)> {
    let mut cuts: Vec<usize> = name.match_indices(HELPER_SEPARATOR).map(|(i, _)| i).collect();
    cuts.reverse();
    for i in cuts {
        let owner = name[..i].trim();
        let label = name[i + HELPER_SEPARATOR.len_utf8()..].trim();
        if !owner.is_empty() && !label.is_empty() && is_known(owner) {
            return Some((owner, label));
        }
    }
    None
}

/// `instructions` for an ordinary agent: who it is here, and how the scripts
/// and subagents it starts must introduce themselves.
///
/// This is where the rule lives. Every MCP client hands `instructions` to its
/// model on connect, so the agent reads it at the moment it matters and with
/// its own name already filled in -- which it otherwise has no way to know,
/// because the name sits in a config header the model never sees.
pub fn agent_instructions(name: &str) -> String {
    format!(
        "{base} You are connected to Patchbay as '{name}'. If you write a script or launch a \
         subagent that talks to this gateway by itself (its own HTTP or MCP connection, not your \
         tool calls), never invent a client name for it: it must send the HTTP header \
         `X-Patchbay-Client: {name}/<short-label>` on initialize, plus `X-Patchbay-Jacks: \
         <comma-separated server names it needs>` (or `*` for all of yours). Such a helper needs \
         no user approval, adds no entry to the user's agent list, gets no servers unless you \
         name them, and can never reach more than you can.",
        base = BASE_INSTRUCTIONS,
        name = name
    )
}

/// `instructions` for a helper: what it is, what it was granted, and how to
/// ask for more. `configured` is every jack name in the config, so a typo in
/// the header is reported instead of silently granting nothing.
pub fn helper_instructions(owner: &str, helper: &HelperInfo, configured: &[String]) -> String {
    let mut text = format!(
        "{} This session is the helper '{}' of '{}'.",
        BASE_INSTRUCTIONS, helper.label, owner
    );
    match &helper.grant {
        JackGrant::All => {
            text.push_str(&format!(
                " It was granted every server '{}' has (X-Patchbay-Jacks: *).",
                owner
            ));
        }
        JackGrant::Only(names) if names.is_empty() => {
            text.push_str(
                " It was granted NO servers, so it has no tools: a helper gets only what it asks \
                 for. Reconnect with the HTTP header `X-Patchbay-Jacks: <comma-separated server \
                 names>` (or `*` for all of its owner's) on initialize.",
            );
        }
        JackGrant::Only(names) => {
            let (known, unknown): (Vec<&String>, Vec<&String>) = names
                .iter()
                .partition(|n| n.as_str() == META_TOOLS_GRANT || configured.contains(*n));
            if !known.is_empty() {
                text.push_str(&format!(" Servers granted to it: {}.", join(&known)));
            }
            if !unknown.is_empty() {
                text.push_str(&format!(
                    " Not configured in Patchbay, so ignored: {}.",
                    join(&unknown)
                ));
            }
        }
    }
    text.push_str(&format!(
        " A granted server is available only while '{}' itself has it.",
        owner
    ));
    text
}

fn join(names: &[&String]) -> String {
    names
        .iter()
        .map(|n| n.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The `initialize` refusal for a name Patchbay has never seen, when the user
/// asked to approve new agents.
///
/// It replaces a system-modal Allow/Deny dialog. That dialog asked the wrong
/// person the wrong question: for an invented script name both answers were
/// bad -- Yes made a permanent agent with the wrong permissions, No made a
/// permanent forbidden entry and a broken script. The right question is "whose
/// are you?", and only the caller can answer it. So the caller is told how.
pub fn unknown_client_refusal(name: &str) -> String {
    format!(
        "Patchbay does not know a client named '{name}', and it is set to ask the user before a \
         new agent gets in, so no session was started. If you are a script or a subagent started \
         by an agent that already uses Patchbay, do not invent a name: send the HTTP header \
         `X-Patchbay-Client: <owner>/<short-label>` on initialize, where <owner> is the exact \
         name your owner connects under (the X-Patchbay-Client value in its own MCP config for \
         this URL), plus `X-Patchbay-Jacks: <comma-separated server names you need>` (or `*`); \
         without that second header a helper gets no servers. If '{name}' really is a new agent, \
         ask the user to let it in (Patchbay window > Agents > Waiting, or the tray menu > New \
         agents waiting) and connect again.",
        name = name
    )
}

/// Model-directed text for a helper calling a server it was not granted.
pub fn not_granted_text(owner: &str, label: &str, what: &str) -> String {
    format!(
        "Patchbay: this session is the helper '{}' of '{}' and was not granted {}. A helper only \
         gets the servers named in the X-Patchbay-Jacks header of its initialize request \
         (comma-separated names, or * for every server its owner has). Do not retry; reconnect \
         with that header if this tool is required.",
        label, owner, what
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known(names: &'static [&'static str]) -> impl Fn(&str) -> bool {
        move |n| names.contains(&n)
    }

    #[test]
    fn a_name_without_a_separator_is_not_a_helper() {
        assert_eq!(split_helper("Codex", known(&["Codex"])), None);
    }

    #[test]
    fn a_known_owner_makes_the_rest_a_label() {
        assert_eq!(
            split_helper("Claude Code - Personal/bee-script", known(&["Claude Code - Personal"])),
            Some(("Claude Code - Personal", "bee-script"))
        );
    }

    #[test]
    fn an_unknown_owner_leaves_the_name_an_ordinary_identity() {
        // A real client may be called `@scope/tool`. It must not become a
        // helper of a `@scope` nobody has ever seen.
        assert_eq!(split_helper("@scope/tool", known(&["Codex"])), None);
    }

    #[test]
    fn empty_halves_are_not_a_helper() {
        assert_eq!(split_helper("Codex/", known(&["Codex"])), None);
        assert_eq!(split_helper("/label", known(&[""])), None);
        assert_eq!(split_helper("Codex/  ", known(&["Codex"])), None);
    }

    #[test]
    fn the_longest_known_owner_wins() {
        assert_eq!(
            split_helper("org/tool/script", known(&["org", "org/tool"])),
            Some(("org/tool", "script"))
        );
        // Only the short one is known: the rest, separator included, is the label.
        assert_eq!(
            split_helper("org/tool/script", known(&["org"])),
            Some(("org", "tool/script"))
        );
    }

    #[test]
    fn no_header_grants_nothing() {
        // The default is the whole point: a forgotten header is no access.
        for header in [None, Some(""), Some("  "), Some(",,")] {
            let grant = JackGrant::parse(header);
            assert_eq!(grant, JackGrant::Only(BTreeSet::new()), "header {:?}", header);
            assert!(!grant.allows("prod"));
            assert!(!grant.allows(META_TOOLS_GRANT));
        }
    }

    #[test]
    fn a_list_grants_exactly_its_names() {
        let grant = JackGrant::parse(Some(" bee-memory-bank , limits "));
        assert!(grant.allows("bee-memory-bank"));
        assert!(grant.allows("limits"));
        assert!(!grant.allows("prod"));
        assert!(!grant.allows(META_TOOLS_GRANT));
        assert_eq!(grant.describe(), "bee-memory-bank, limits");
    }

    #[test]
    fn a_star_grants_everything_wherever_it_stands() {
        for header in ["*", " * ", "limits, *"] {
            let grant = JackGrant::parse(Some(header));
            assert_eq!(grant, JackGrant::All, "header {:?}", header);
            assert!(grant.allows("anything"));
            assert!(grant.allows(META_TOOLS_GRANT));
        }
    }

    #[test]
    fn names_that_cannot_be_a_jack_are_dropped() {
        // Markup must not ride into the window on a header.
        let grant = JackGrant::parse(Some("<img src=x>, limits, a__b"));
        assert_eq!(grant.describe(), "limits");
    }

    #[test]
    fn the_agent_instructions_carry_the_agents_own_name() {
        let text = agent_instructions("Claude Code - Personal");
        assert!(text.starts_with(BASE_INSTRUCTIONS));
        assert!(text.contains("X-Patchbay-Client: Claude Code - Personal/<short-label>"));
        assert!(text.contains("X-Patchbay-Jacks"));
    }

    #[test]
    fn the_helper_instructions_say_what_was_granted() {
        let configured = vec!["limits".to_string(), "prod".to_string()];
        let none = HelperInfo {
            label: "probe".to_string(),
            grant: JackGrant::parse(None),
        };
        let text = helper_instructions("Codex", &none, &configured);
        assert!(text.contains("helper 'probe' of 'Codex'"));
        assert!(text.contains("NO servers"));
        assert!(text.contains("X-Patchbay-Jacks"));

        let some = HelperInfo {
            label: "probe".to_string(),
            grant: JackGrant::parse(Some("limits, typo")),
        };
        let text = helper_instructions("Codex", &some, &configured);
        assert!(text.contains("Servers granted to it: limits."));
        assert!(text.contains("ignored: typo."));
    }

    #[test]
    fn the_refusal_tells_a_script_how_to_introduce_itself() {
        let text = unknown_client_refusal("orchestrator");
        assert!(text.contains("'orchestrator'"));
        assert!(text.contains("X-Patchbay-Client: <owner>/<short-label>"));
        assert!(text.contains("X-Patchbay-Jacks"));
        assert!(text.contains("Agents > Waiting"), "a real new agent needs a way in: {}", text);
    }
}
