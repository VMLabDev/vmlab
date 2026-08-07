//! **`attachable`**, and the words every rung of §19.4's failure ladder says.
//!
//! A machine whose agent predates §19 is still a perfectly good machine, so
//! the failure lands where it costs least and says most:
//!
//! - **`validate` says nothing.** It is a config check with no side effects,
//!   and the only statically available signal is the template's sealed
//!   `agent_version` — a free-form string. Comparing it is *inference*, which
//!   the capability doctrine rejects. There is deliberately nothing in this
//!   module for `validate` to call.
//! - **`up` warns** ([`warning`]). The handshake is part of readiness, so by
//!   then the features are honestly probed: free, correctly sourced, early.
//! - **Attach fails hard** ([`refusal`]), naming both remedies — rebuild the
//!   template, or push the shipped agent into the running machine. The facade
//!   is a general capability, so a machine that cannot be attached to is still
//!   a perfectly good machine and `up` failing over it would be
//!   disproportionate.
//!
//! [`attachable`] is **exactly `tunnel && fileops`** — *this agent can serve
//! an attach*, never *your attach will succeed*. The narrow definition is
//! load-bearing: identity is declared separately (§19.2), and a flag promising
//! success would become a lie. It is a computed projection over probed facts
//! (ADR-0004), never an inference from machine kind, which is also why a
//! template built with the agent disabled reports the same `false` through the
//! same path as one whose agent is merely old.
//!
//! It deliberately does **not** widen to cover `watch`: the workspace syncer
//! checks `watch && fileops`, a different consumer with a different answer
//! (§19.6).

use vmlab_agent_proto::features;

/// The agent features an attach needs, and no others (§19.4).
pub const ATTACH_FEATURES: [&str; 2] = [features::TUNNEL, features::FILEOPS];

/// Whether an agent advertising `features` can serve an attach.
///
/// The one derivation. Every surface that reports `attachable` — the lab
/// status projection, `vmlab machine capabilities`, the facade's per-channel
/// refusals — computes it here rather than re-deriving it from a feature list
/// of its own.
pub fn attachable(features: &[String]) -> bool {
    missing(features).is_empty()
}

/// Which attach features this agent does not advertise, in [`ATTACH_FEATURES`]
/// order. Empty for an agent that can serve an attach; both for a machine with
/// no agent answering at all, which is the honest answer — an absent agent
/// advertises nothing.
pub fn missing(features: &[String]) -> Vec<&'static str> {
    ATTACH_FEATURES
        .into_iter()
        .filter(|want| !features.iter().any(|f| f == want))
        .collect()
}

/// What a refused attach-grade request tells the developer: what the agent
/// does not serve, and **both** remedies.
///
/// `what` is the thing being refused, in the words its own protocol spells it
/// (`` `sftp` ``, `` `direct-tcpip` ``, `an attach`). `machine` is `None` for
/// a caller holding only an agent channel — the agent client does not know
/// which machine it is talking to — and the verb then carries a placeholder
/// rather than a name that would be a guess.
pub fn refusal(machine: Option<&str>, what: &str, missing: &[&str]) -> String {
    format!(
        "{what}: {} — {} (§19.4)",
        serves_no(machine, missing),
        remedies(machine)
    )
}

/// §19.4's middle rung: what `up` prints for a machine whose agent answered
/// and cannot serve an attach. `None` when it can.
///
/// A warning and never a failure: the facade is a general capability, and a
/// shell on this machine still works.
pub fn warning(machine: &str, features: &[String]) -> Option<String> {
    let missing = missing(features);
    if missing.is_empty() {
        return None;
    }
    Some(format!(
        "warning: {} — a shell still works, but nothing can attach to it \
         until you {} (§19.4)",
        serves_no(Some(machine), &missing),
        remedies(Some(machine)),
    ))
}

/// "…'s agent serves no `fileops`" — the clause naming what is missing.
fn serves_no(machine: Option<&str>, missing: &[&str]) -> String {
    let whose = match machine {
        Some(m) => format!("\"{m}\"'s agent"),
        None => "this guest's agent".to_string(),
    };
    format!("{whose} serves no {}", feature_list(missing))
}

/// Both remedies, in the order to consider them: the rebuild that is policy,
/// then the repair verb that is a tool (§19.4).
fn remedies(machine: Option<&str>) -> String {
    format!(
        "rebuild the template to bake in the agent this vmlab ships, or push that agent \
         into the running machine with `vmlab machine repair-agent {}`",
        machine.unwrap_or("<machine>")
    )
}

/// `` `tunnel` and `fileops` ``, in the order given.
fn feature_list(features: &[&str]) -> String {
    let quoted: Vec<String> = features.iter().map(|f| format!("`{f}`")).collect();
    match quoted.split_last() {
        None => "nothing".to_string(),
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn features(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    /// The definition, and the whole definition: both features, and nothing
    /// about identity, readiness or machine kind.
    #[test]
    fn attachable_is_exactly_tunnel_and_fileops() {
        assert!(attachable(&features(&["tunnel", "fileops"])));
        assert!(attachable(&features(&[
            "terminal", "exec", "fileops", "tunnel", "metrics"
        ])));
        assert!(!attachable(&features(&["tunnel"])));
        assert!(!attachable(&features(&["fileops"])));
        assert!(!attachable(&features(&["terminal", "exec"])));
    }

    /// `watch` is the workspace syncer's question, not this one: an agent that
    /// serves the attach pair is attachable whether or not it watches, and one
    /// that only watches is not attachable at all (§19.6).
    #[test]
    fn attachable_does_not_widen_to_cover_watch() {
        assert!(attachable(&features(&["tunnel", "fileops"])));
        assert!(attachable(&features(&["tunnel", "fileops", "watch"])));
        assert!(!attachable(&features(&["watch", "fileops"])));
        assert!(!ATTACH_FEATURES.contains(&"watch"));
    }

    /// A machine with no agent answering advertises nothing, so it is missing
    /// both — the same answer, by the same path, as a template built with the
    /// agent disabled.
    #[test]
    fn no_agent_answering_is_missing_both() {
        assert_eq!(missing(&[]), ["tunnel", "fileops"]);
        assert!(!attachable(&[]));
    }

    /// Every refusal names what is missing *and* both remedies: a developer
    /// who reads one of these should never have to ask what to do next.
    #[test]
    fn a_refusal_names_the_gap_and_both_remedies() {
        let out = refusal(Some("dev01"), "`sftp`", &["fileops"]);
        assert!(out.starts_with("`sftp`:"), "{out}");
        assert!(
            out.contains("\"dev01\"'s agent serves no `fileops`"),
            "{out}"
        );
        assert!(out.contains("rebuild the template"), "{out}");
        assert!(
            out.contains("`vmlab machine repair-agent dev01`"),
            "the repair verb, aimed at the machine: {out}"
        );
        assert!(!out.contains("tunnel"), "only what is missing: {out}");
    }

    /// The agent client knows the channel, not the machine on the other end of
    /// it, so its refusals carry a placeholder rather than inventing a name.
    #[test]
    fn a_refusal_without_a_machine_carries_a_placeholder() {
        let out = refusal(None, "a tunnel", &["tunnel"]);
        assert!(
            out.contains("this guest's agent serves no `tunnel`"),
            "{out}"
        );
        assert!(out.contains("repair-agent <machine>"), "{out}");
    }

    /// Both missing reads as a list, not as two sentences.
    #[test]
    fn a_refusal_lists_every_missing_feature() {
        let out = refusal(Some("dev01"), "an attach", &missing(&[]));
        assert!(out.contains("serves no `tunnel` and `fileops`"), "{out}");
    }

    /// `up`'s rung: it warns, it says a shell still works, and it says nothing
    /// at all about a machine that can serve an attach.
    #[test]
    fn up_warns_without_condemning_the_machine() {
        let said = warning("dev01", &features(&["terminal", "exec", "tunnel"])).unwrap();
        assert!(said.starts_with("warning: "), "{said}");
        assert!(said.contains("serves no `fileops`"), "{said}");
        assert!(said.contains("a shell still works"), "{said}");
        assert!(said.contains("repair-agent dev01"), "{said}");

        assert_eq!(warning("dev01", &features(&["tunnel", "fileops"])), None);
    }
}
