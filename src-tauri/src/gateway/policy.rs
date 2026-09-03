//! (S14) What a caller reaching Patchbay over the network may change.
//!
//! Patchbay's whole purpose is that the human decides which MCP servers an
//! agent can reach. Two doors let a caller decide that for themselves:
//!
//! - the gateway-owned meta tools (`patchbay__toggle_jack`, `patchbay__add_jack`),
//!   callable by any agent that has completed `initialize`, and
//! - the typed REST routes under `/admin/jacks*`, which carry **no identity at
//!   all** — anything that can open a socket to `127.0.0.1` can call them, which
//!   on a developer machine is a great many things.
//!
//! Through either door, an agent could switch a server back ON that the user
//! had deliberately switched off — including a production database server —
//! and Patchbay would faithfully start it and hand over the tools. Every other
//! enforcement path in the app (the Custom list, the forbidden gate, the
//! approval dialog) was being decided over.
//!
//! The rule, stated once and enforced at all four call sites:
//!
//! > **Nothing arriving over the gateway may make a server reachable that was
//! > not reachable already.**
//!
//! Turning a server OFF stays allowed: that only ever reduces what the caller
//! can reach, it is how an agent politely drops a server it does not need, and
//! the user can always turn it back on. Listing stays allowed. Only the
//! direction that GRANTS is refused, and it is refused with a message that says
//! where the human control is, so the model tells the user what to click
//! instead of retrying.
//!
//! This is deliberately not an authentication scheme. Identifying loopback
//! callers is a much larger question; this closes the door that was actually
//! open, without pretending to answer it.

/// Refuse an agent-initiated request to switch a jack ON.
///
/// `Ok(())` for `want_on == false` — turning a server off is always the
/// caller's to do.
pub fn check_enable(jack: &str, want_on: bool) -> Result<(), String> {
    if !want_on {
        return Ok(());
    }
    Err(format!(
        "Patchbay: agents may switch a server OFF but not ON, so '{}' was not \
         enabled. Turning a server on is the user's decision — ask them to \
         enable '{}' from the Patchbay tray icon (or its window), and it will \
         appear for you without reconnecting.",
        jack, jack
    ))
}

/// Refuse an agent-initiated `add_jack` that would start the new server
/// immediately. Adding it switched OFF is allowed: that writes a definition the
/// user can review and enable, and reaches nothing on its own.
pub fn check_add(jack: &str, patched: bool) -> Result<(), String> {
    if !patched {
        return Ok(());
    }
    Err(format!(
        "Patchbay: agents may not add a server that is already switched on. \
         Re-send this with \"patched\": false and the definition for '{}' will \
         be saved switched off, ready for the user to enable from the tray icon \
         (or its window).",
        jack
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switching_off_is_always_allowed() {
        assert!(check_enable("prod", false).is_ok());
        assert!(check_add("prod", false).is_ok());
    }

    #[test]
    fn switching_on_is_refused_and_says_where_the_human_control_is() {
        let err = check_enable("prod-db", true).expect_err("must refuse");
        assert!(err.contains("prod-db"), "the message must name the jack");
        assert!(
            err.contains("tray"),
            "a refusal the model cannot act on just becomes a retry loop: {}",
            err
        );
    }

    #[test]
    fn adding_a_server_already_switched_on_is_refused() {
        let err = check_add("prod-db", true).expect_err("must refuse");
        assert!(
            err.contains("\"patched\": false"),
            "the refusal must state the exact way to comply: {}",
            err
        );
    }
}
