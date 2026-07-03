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
  "agile", "alabaster", "alpine", "amber", "arctic", "ardent", "artful", "ashen", "astral", "atomic",
  "auburn", "aurora", "autumn", "azure", "balmy", "beige", "blazing", "bold", "bounding", "brass",
  "brave", "breezy", "bright", "briny", "brisk", "bronze", "calm", "canny", "cavernous", "cerulean",
  "chartreuse", "cheery", "chipper", "clever", "cloudy", "coastal", "cobalt", "copper", "coral", "cosmic",
  "crafty", "craggy", "crimson", "crisp", "crystal", "cyan", "dapper", "dappled", "daring", "dashing",
  "dauntless", "dazzling", "deft", "desert", "dewy", "doughty", "dreamy", "drifting", "dusky", "eager",
  "earnest", "earthen", "ebony", "elder", "electric", "ember", "emerald", "fabled", "fabulous", "fearless",
  "feisty", "feral", "fervent", "fiery", "fleet", "flinty", "foggy", "frank", "frosty", "gallant",
  "garnet", "genial", "gentle", "giant", "giddy", "gilded", "gilt", "glacial", "glassy", "gleaming",
  "glowing", "golden", "grand", "granite", "gusty", "hallowed", "hardy", "hazy", "hearty", "heroic",
  "hidden", "hushed", "hyper", "icy", "indigo", "iron", "ivory", "jade", "jagged", "jaunty",
  "jolly", "jovial", "jubilant", "keen", "khaki", "kindly", "leathern", "lilac", "little", "lively",
  "lofty", "loyal", "lucid", "lucky", "luminous", "lunar", "lush", "magenta", "marble", "maroon",
  "mauve", "mega", "mellow", "mercurial", "merry", "meteoric", "mighty", "mini", "mirthful", "misty",
  "modest", "molten", "mossy", "mythic", "neon", "nimble", "noble", "obsidian", "oceanic", "ochre",
  "olive", "onyx", "opal", "opaline", "patient", "pearl", "pearly", "peppy", "periwinkle", "petite",
  "placid", "playful", "plucky", "plum", "plush", "poised", "polar", "prancing", "prime", "proud",
  "quartz", "quick", "quiet", "quirky", "radiant", "rakish", "rapid", "ready", "regal", "regnal",
  "rippling", "robust", "rocky", "rose", "roving", "ruby", "rugged", "russet", "saffron", "sage",
  "sandy", "sapphire", "satin", "savvy", "scarlet", "seafaring", "serene", "shadow", "shady", "shimmering",
  "shining", "sienna", "silent", "silken", "silver", "sincere", "slate", "sleek", "slender", "snappy",
  "snowy", "soaring", "solar", "sparkling", "spirited", "springy", "spry", "stalwart", "starry", "stately",
  "steady", "steely", "stellar", "stoic", "stony", "stormy", "stout", "sturdy", "suave", "sublime",
  "sunlit", "sunny", "super", "swift", "tangerine", "tawny", "teal", "tender", "thunder", "tidal",
  "towering", "tranquil", "trusty", "turbo", "turquoise", "twilight", "ultra", "umber", "upbeat", "valiant",
  "velvet", "verdant", "vermilion", "vernal", "vibrant", "violet", "vivid", "volcanic", "wandering", "waxen",
  "whimsical", "wily", "windswept", "wintry", "wise", "wispy", "witty", "woolen", "zealous", "zephyr",
  "zesty", "zippy",
] as const;

const NOUNS = [
  "aardvark", "adder", "agama", "albatross", "alder", "alpaca", "anchovy", "angelfish", "anole", "ant",
  "antelope", "aphid", "aspen", "atoll", "aurora", "avocet", "badger", "barracuda", "basilisk", "basin",
  "bay", "bear", "beaver", "beetle", "birch", "bison", "bittern", "blenny", "bluejay", "bluff",
  "boa", "bobcat", "bongo", "boulder", "bream", "brill", "brook", "buffalo", "bumblebee", "bunting",
  "butte", "butterfly", "buzzard", "caiman", "canyon", "cape", "capybara", "caracal", "cardinal", "caribou",
  "carp", "cavern", "cedar", "chafer", "chameleon", "chamois", "cheetah", "chickadee", "chipmunk", "chub",
  "cicada", "cinder", "civet", "cliff", "cobra", "cod", "comet", "condor", "conger", "cormorant",
  "cosmos", "cougar", "cove", "coyote", "crab", "crag", "crane", "creek", "cricket", "crocodile",
  "curlew", "dab", "dace", "damselfly", "dell", "delta", "dhole", "dingo", "dolphin", "dorado",
  "dormouse", "dove", "dragonfly", "dune", "eagle", "earwig", "eclipse", "egret", "eland", "elk",
  "elm", "ember", "falcon", "fen", "fennec", "fern", "ferret", "finch", "firefly", "fisher",
  "fjord", "flamingo", "flounder", "fossa", "fox", "galaxy", "gaur", "gazelle", "gecko", "gemsbok",
  "genet", "gerbil", "geyser", "glacier", "glade", "glen", "glowworm", "gnat", "godwit", "goldfinch",
  "goose", "gorge", "grasshopper", "grebe", "grotto", "grouper", "grouse", "gudgeon", "gulch", "gull",
  "guppy", "haddock", "hake", "halibut", "harbor", "hare", "harrier", "hawk", "hedgehog", "heron",
  "herring", "hoopoe", "hornet", "hyena", "ibex", "ibis", "iguana", "impala", "isle", "ivy",
  "jackal", "jackdaw", "jaguar", "jay", "jerboa", "junco", "katydid", "kestrel", "kingfisher", "kite",
  "knoll", "koi", "komodo", "krait", "kudu", "ladybug", "lagoon", "lapwing", "lark", "ledge",
  "lemur", "leopard", "ling", "linnet", "lion", "lizard", "loach", "lobster", "locust", "lynx",
  "macaw", "mackerel", "magpie", "mallard", "mamba", "manta", "mantis", "maple", "marlin", "marmot",
  "marsh", "marten", "martin", "mayfly", "meadow", "meerkat", "merlin", "mesa", "meteor", "midge",
  "mink", "minnow", "mole", "mongoose", "monitor", "moor", "moose", "moth", "mullet", "muntjac",
  "narwhal", "nebula", "neutron", "newt", "nightingale", "nightjar", "nova", "nuthatch", "nyala", "oak",
  "oasis", "ocelot", "okapi", "onager", "orbit", "orca", "oriole", "oryx", "osprey", "otter",
  "ouzel", "owl", "pangolin", "panther", "parakeet", "pass", "peacock", "pebble", "pelican", "penguin",
  "perch", "petrel", "pheasant", "photon", "pigeon", "pike", "pine", "pipit", "plaice", "plateau",
  "plover", "polecat", "pollock", "porcupine", "porpoise", "prairie", "prawn", "pronghorn", "proton", "ptarmigan",
  "puffin", "pulsar", "puma", "python", "quail", "quasar", "quokka", "quoll", "raccoon", "ratel",
  "raven", "redstart", "reef", "reindeer", "ridge", "rill", "roach", "robin", "roller", "rook",
  "sable", "saiga", "salamander", "salmon", "sanderling", "sandpiper", "sardine", "savanna", "sawfish", "scarab",
  "sculpin", "seahorse", "seal", "serow", "serval", "shad", "shark", "shearwater", "shoal", "shrew",
  "shrike", "siskin", "skate", "skink", "skua", "skylark", "sloth", "slowworm", "snapper", "snipe",
  "sole", "sparrow", "spire", "sprat", "spring", "springbok", "squirrel", "stag", "starling", "steppe",
  "stoat", "stork", "strait", "sturgeon", "summit", "sunfish", "swallow", "swan", "swordfish", "taipan",
  "tanager", "tapir", "tarn", "tench", "tern", "terrapin", "thrush", "tiger", "toad", "tortoise",
  "toucan", "trout", "tuatara", "tuna", "tundra", "turbot", "turnstone", "turtle", "urchin", "vale",
  "viper", "vireo", "vole", "vulture", "wagtail", "wahoo", "wallaby", "walleye", "walrus", "warbler",
  "warthog", "waxwing", "weasel", "weevil", "wigeon", "wildcat", "willow", "wolf", "wolverine", "wombat",
  "woodpecker", "wrasse", "wren", "yak", "zebra", "zenith",
] as const;
