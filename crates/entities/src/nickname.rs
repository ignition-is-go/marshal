//! Deterministic, stateless session nicknames.
//!
//! [`nickname`] maps a session id to a memorable `adjective-noun` handle
//! (e.g. `swift-falcon`). It is a PURE function of the id: every process
//! (statusline, shim roster/whoami, daemon, web) computes the same name with
//! no coordination, no stored field, and no collision-suffixing.
//!
//! That statelessness is deliberate. The previous nickname scheme assigned
//! names centrally and had to "rebalance suffixed survivors" when a session
//! disconnected (see the removed daemon saga) — central, stateful, racy, and
//! the reason nicknames were torn out. Deriving the name from the id sidesteps
//! all of it: stable for the session's life, survives reconnect/daemon-restart,
//! and reproducible everywhere.
//!
//! Collisions (two ids hashing to the same pair) are POSSIBLE but not MANAGED.
//! With the wordlists below there are `ADJECTIVES.len() * NOUNS.len()` combos,
//! so a clash among a few dozen live sessions is rare. Recipient resolution
//! treats an ambiguous nickname as an error and callers fall back to the
//! session id — we never suffix to force uniqueness, which would reintroduce
//! the stateful rebalance.

/// 64-bit FNV-1a — a fixed, platform-independent hash so a session's nickname
/// is identical across processes and releases. (`std`'s `DefaultHasher` is
/// explicitly NOT stable across versions, so it can't be used here.)
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Memorable `adjective-noun` handle derived deterministically from
/// `session_id`. Empty id → `"anon"`.
pub fn nickname(session_id: &str) -> String {
    if session_id.is_empty() {
        return "anon".to_string();
    }
    let h = fnv1a(session_id.as_bytes());
    let adj = ADJECTIVES[(h % ADJECTIVES.len() as u64) as usize];
    // A different slice of the hash for the noun so the two indices don't
    // correlate when the list lengths share common factors.
    let noun = NOUNS[((h >> 32) % NOUNS.len() as u64) as usize];
    format!("{adj}-{noun}")
}

const ADJECTIVES: &[&str] = &[
    "swift", "brave", "calm", "bright", "bold", "clever", "cosmic", "crisp", "daring", "deft",
    "eager", "electric", "fabled", "fearless", "fleet", "gallant", "gentle", "giddy", "golden",
    "grand", "hardy", "hidden", "jolly", "keen", "lucky", "lunar", "mellow", "merry", "mighty",
    "nimble", "noble", "polar", "prime", "proud", "quiet", "quick", "rapid", "regal", "rugged",
    "sleek", "snappy", "solar", "spry", "stellar", "sturdy", "sunny", "tidal", "vivid", "witty",
    "zesty", "amber", "azure", "coral", "crimson", "ivory", "jade", "scarlet", "teal", "violet",
    "cobalt", "frosty", "ember", "shadow", "silent", "stormy", "misty", "dusky", "autumn", "wintry",
    "arctic", "alpine", "coastal", "desert", "marble", "granite", "opal", "onyx", "pearl", "ruby",
    "velvet", "copper", "silver", "brass", "steely", "neon", "atomic", "turbo", "hyper", "mega",
    "ultra", "super", "lively", "loyal", "patient", "plucky", "radiant", "serene", "spirited", "valiant",
];

const NOUNS: &[&str] = &[
    "falcon", "otter", "badger", "lynx", "heron", "raven", "fox", "wolf", "bear", "hawk", "owl",
    "eagle", "sparrow", "finch", "robin", "crane", "swan", "ibis", "kestrel", "osprey", "puffin",
    "marten", "stoat", "weasel", "ferret", "beaver", "mole", "hare", "bison", "moose", "elk",
    "stag", "ibex", "oryx", "gazelle", "antelope", "panther", "jaguar", "leopard", "cougar",
    "ocelot", "serval", "caracal", "cheetah", "tiger", "lion", "cobra", "viper", "python", "gecko",
    "iguana", "newt", "toad", "turtle", "tortoise", "dolphin", "orca", "narwhal", "walrus", "seal",
    "manta", "marlin", "tuna", "perch", "pike", "trout", "salmon", "koi", "urchin", "prawn", "crab",
    "lobster", "mantis", "beetle", "cricket", "cicada", "firefly", "moth", "hornet", "comet", "nova",
    "quasar", "pulsar", "nebula", "meteor", "photon", "proton", "neutron", "ember", "cinder",
    "pebble", "boulder", "canyon", "summit", "glacier", "fjord", "delta", "reef", "dune", "mesa",
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn is_deterministic() {
        let id = "c80daf7d-2f6f-40cc-8cdf-fae30a5d842a";
        assert_eq!(nickname(id), nickname(id));
    }

    #[test]
    fn shape_is_adjective_dash_noun() {
        let n = nickname("21506801-20f9-4673-bb4d-a9d94ab8de3d");
        let (adj, noun) = n.split_once('-').expect("has a dash");
        assert!(ADJECTIVES.contains(&adj), "adj `{adj}` from wordlist");
        assert!(NOUNS.contains(&noun), "noun `{noun}` from wordlist");
    }

    #[test]
    fn empty_is_anon() {
        assert_eq!(nickname(""), "anon");
    }

    #[test]
    fn distinct_ids_mostly_distinct_names() {
        // Not a uniqueness guarantee (collisions are allowed by design), but a
        // sanity check that the hash spreads: 100 distinct uuids should yield
        // close to 100 distinct names with these wordlists.
        let names: HashSet<String> = (0..100)
            .map(|i| nickname(&format!("00000000-0000-0000-0000-{i:012x}")))
            .collect();
        assert!(names.len() >= 95, "expected good spread, got {}", names.len());
    }
}
