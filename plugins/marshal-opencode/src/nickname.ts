// Deterministic adjective-noun session nickname — an EXACT port of marshal's
// Rust `nickname()` (crates/entities/src/nickname.rs) so the handle this plugin
// shows matches what the daemon, roster, and every other agent compute for the
// same session id. The `Session` entity doesn't carry the rendered nickname
// (it's derived everywhere, never stored), so the plugin recomputes it.
//
// 64-bit FNV-1a via BigInt — JS numbers can't hold the u64 wrapping multiply.
// The wordlists below are copied verbatim from the Rust source; keep them in
// lockstep or nicknames will diverge.

const MASK_64 = 0xffffffffffffffffn;
const FNV_OFFSET = 0xcbf29ce484222325n;
const FNV_PRIME = 0x00000100000001b3n;

function fnv1a(s: string): bigint {
  let h = FNV_OFFSET;
  for (const b of new TextEncoder().encode(s)) {
    h = (h ^ BigInt(b)) & MASK_64;
    h = (h * FNV_PRIME) & MASK_64;
  }
  return h;
}

/** `adjective-noun` handle derived deterministically from `sessionId`.
 *  Empty id → `"anon"`. Matches the Rust daemon byte-for-byte. */
export function nickname(sessionId: string): string {
  if (!sessionId) return "anon";
  const h = fnv1a(sessionId);
  const adj = ADJECTIVES[Number(h % BigInt(ADJECTIVES.length))];
  const noun = NOUNS[Number((h >> 32n) % BigInt(NOUNS.length))];
  return `${adj}-${noun}`;
}

const ADJECTIVES = [
  "swift", "brave", "calm", "bright", "bold", "clever", "cosmic", "crisp", "daring", "deft",
  "eager", "electric", "fabled", "fearless", "fleet", "gallant", "gentle", "giddy", "golden",
  "grand", "hardy", "hidden", "jolly", "keen", "lucky", "lunar", "mellow", "merry", "mighty",
  "nimble", "noble", "polar", "prime", "proud", "quiet", "quick", "rapid", "regal", "rugged",
  "sleek", "snappy", "solar", "spry", "stellar", "sturdy", "sunny", "tidal", "vivid", "witty",
  "zesty", "amber", "azure", "coral", "crimson", "ivory", "jade", "scarlet", "teal", "violet",
  "cobalt", "frosty", "ember", "shadow", "silent", "stormy", "misty", "dusky", "autumn",
  "wintry", "arctic", "alpine", "coastal", "desert", "marble", "granite", "opal", "onyx",
  "pearl", "ruby", "velvet", "copper", "silver", "brass", "steely", "neon", "atomic", "turbo",
  "hyper", "mega", "ultra", "super", "lively", "loyal", "patient", "plucky", "radiant", "serene",
  "spirited", "valiant",
] as const;

const NOUNS = [
  "falcon", "otter", "badger", "lynx", "heron", "raven", "fox", "wolf", "bear", "hawk", "owl",
  "eagle", "sparrow", "finch", "robin", "crane", "swan", "ibis", "kestrel", "osprey", "puffin",
  "marten", "stoat", "weasel", "ferret", "beaver", "mole", "hare", "bison", "moose", "elk",
  "stag", "ibex", "oryx", "gazelle", "antelope", "panther", "jaguar", "leopard", "cougar",
  "ocelot", "serval", "caracal", "cheetah", "tiger", "lion", "cobra", "viper", "python", "gecko",
  "iguana", "newt", "toad", "turtle", "tortoise", "dolphin", "orca", "narwhal", "walrus", "seal",
  "manta", "marlin", "tuna", "perch", "pike", "trout", "salmon", "koi", "urchin", "prawn",
  "crab", "lobster", "mantis", "beetle", "cricket", "cicada", "firefly", "moth", "hornet",
  "comet", "nova", "quasar", "pulsar", "nebula", "meteor", "photon", "proton", "neutron",
  "ember", "cinder", "pebble", "boulder", "canyon", "summit", "glacier", "fjord", "delta",
  "reef", "dune", "mesa",
] as const;
