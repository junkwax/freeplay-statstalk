//! Which game a result or a query belongs to.
//!
//! Ratings are per game: a Mortal Kombat II rating says nothing about how
//! someone plays NBA Jam, so the two never share a Glicko pool.
//!
//! The game rides inside `rom_hash`, which the client already sends to the
//! signaling server and the signaling server already forwards here untouched.
//! A client running anything but MK2 prefixes it — `nbajam:<fnv>@<core>` —
//! and an unprefixed hash is MK2, because every client before multi-game
//! support sent exactly that. Keeping the key in `rom_hash` means the
//! signaling server needed no change to carry it.

/// The game every pre-multi-game row, request and client belongs to.
pub const DEFAULT_GAME: &str = "mk2";

/// A game key is a short lowercase identifier. Anything else is rejected so
/// a malformed prefix can't mint a rating pool.
fn valid_key(key: &str) -> bool {
    (1..=16).contains(&key.len())
        && key
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// `nbajam:1a2b3c4d@cf53523#9f8e7d6c` -> `nbajam`; `1a2b3c4d@cf53523` -> `mk2`.
pub fn from_rom_hash(rom_hash: &str) -> String {
    match rom_hash.split_once(':') {
        Some((key, _)) if valid_key(key) => key.to_string(),
        _ => DEFAULT_GAME.to_string(),
    }
}

/// `?game=` on a read endpoint. Absent or invalid means MK2, which is what
/// every client that predates the parameter is asking about.
pub fn from_query(game: Option<&str>) -> String {
    match game.map(str::trim) {
        Some(key) if valid_key(key) => key.to_string(),
        _ => DEFAULT_GAME.to_string(),
    }
}

/// Human name for Discord posts.
pub fn display_name(game: &str) -> &str {
    match game {
        "mk2" => "Mortal Kombat II",
        "nbajam" => "NBA Jam",
        "nbahangt" => "NBA Hangtime",
        "umk3w" => "UMK3 Wavenet",
        other => other,
    }
}

/// Games whose results are not posted to Discord: `DISCORD_MUTE_GAMES`, comma-separated keys (e.g. `umk3w`, whose
/// always-on cloud bot would post every match it plays). Unset: every game posts.
pub fn discord_muted(game: &str) -> bool {
    std::env::var("DISCORD_MUTE_GAMES")
        .map(|v| v.split(',').any(|k| k.trim() == game))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unprefixed_hash_is_mk2() {
        assert_eq!(from_rom_hash("1a2b3c4d@cf53523"), "mk2");
        assert_eq!(from_rom_hash("1a2b3c4d@cf53523#9f8e7d6c"), "mk2");
        assert_eq!(from_rom_hash(""), "mk2");
    }

    #[test]
    fn prefixed_hash_names_its_game() {
        assert_eq!(from_rom_hash("nbajam:1a2b3c4d@cf53523#9f8e7d6c"), "nbajam");
    }

    #[test]
    fn malformed_prefix_falls_back_to_mk2() {
        assert_eq!(from_rom_hash("NBA JAM:1a2b@x"), "mk2");
        assert_eq!(from_rom_hash(":1a2b@x"), "mk2");
        assert_eq!(from_query(Some("drop table")), "mk2");
        assert_eq!(from_query(None), "mk2");
        assert_eq!(from_query(Some("nbajam")), "nbajam");
    }
}
