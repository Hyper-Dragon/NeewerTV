//! Every effect the device can be told to display.
//!
//! This is a transcription of what the device's own client sends, not a design.
//! The identifiers are the ones the firmware answers to; the vendor names are
//! whatever that client calls them, reproduced so a Home Assistant user
//! recognises their own app. Nothing here is invented, and nothing here should
//! be renamed to taste — a different name on the wire is a different effect.
//!
//! # Why the published names carry a prefix
//!
//! Home Assistant selects an effect **by name**, and the vendor's own names are
//! not unique across categories: `Meteor` is a display preset *and* a DIY
//! pattern *and* a music mode. A flat list of those would be ambiguous — picking
//! "Meteor" would be picking one of three things at random. So the published
//! name is the vendor name with its category in front where the category is not
//! obvious, and [`all`] is guaranteed to contain no duplicates.
//!
//! Renaming these is safe and changes nothing on the wire; the identifiers are
//! what the device acts on.

/// Which command family an effect belongs to. They share one identifier space
/// on the wire but are sent differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    /// A display preset, sent via the display engine.
    Preset,
    /// Screen-sync. Carries a direction, and `Two-Way Off` ignores it.
    TwoWay,
    /// One of the 24 catalogue scenes.
    Scene,
    /// A user-coloured animation.
    Diy,
    /// Music-reactive mode; a different command entirely.
    Music,
}

/// One selectable effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Effect {
    pub kind: Kind,
    /// The `model` byte for the display engine, or the `modeId` for music.
    pub id: u8,
    /// The name published in `effect_list`, guaranteed unique across [`all`].
    pub name: &'static str,
    /// The vendor application's own label, for recognition and for comments.
    pub vendor_name: &'static str,
    /// Screen-sync direction; `0` for everything that is not [`Kind::TwoWay`].
    pub direction: u8,
}

const fn effect(kind: Kind, id: u8, name: &'static str, vendor_name: &'static str) -> Effect {
    Effect {
        kind,
        id,
        name,
        vendor_name,
        direction: 0,
    }
}

/// The catalogue, transcribed once and never reordered without reason.
///
/// Presets come first because they are what the device's own remote offers;
/// scenes follow, then the DIY patterns and music modes a user is less likely to
/// reach for. Order is only cosmetic — the identifiers are the identity.
const EFFECTS: &[Effect] = &[
    // ── display presets, 0xF8..=0xFF ────────────────────────────────────────
    effect(Kind::Preset, 0xF8, "Ice Grains", "Ice Grains"),
    effect(Kind::Preset, 0xF9, "Rice Snow", "Rice Snow"),
    effect(Kind::Preset, 0xFA, "Northern Lights", "Northern Lights"),
    effect(Kind::Preset, 0xFB, "Starstream", "Starstream"),
    effect(Kind::Preset, 0xFC, "Superfluid", "Superfluid"),
    effect(Kind::Preset, 0xFD, "Meteor", "Meteor"),
    effect(Kind::Preset, 0xFE, "Liquid Crystal", "Liquid Crystal"),
    effect(Kind::Preset, 0xFF, "Southern Lights", "Southern Lights"),
    // ── screen sync ────────────────────────────────────────────────────────
    // One identifier, two behaviours, told apart only by `direction`. The
    // mirrored variant therefore needs its own entry even though the wire value
    // of `model` is identical.
    Effect {
        direction: 0,
        ..effect(Kind::TwoWay, 0xF1, "TV Screen", "TV Screen")
    },
    Effect {
        direction: 1,
        ..effect(Kind::TwoWay, 0xF1, "TV Screen (mirrored)", "TV Screen")
    },
    // `Two-Way Off` ignores direction entirely; the device reports it back as 0.
    effect(Kind::TwoWay, 0xF2, "Two-Way Off", "Two-Way Off"),
    // ── scenes, 0x31..=0x48, four categories of six ────────────────────────
    effect(Kind::Scene, 0x31, "Icelandic Blue", "Icelandic Blue"),
    effect(Kind::Scene, 0x32, "Glacier Express", "Glacier Express"),
    effect(Kind::Scene, 0x33, "Cloud Sea", "Cloud Sea"),
    effect(Kind::Scene, 0x34, "Snow Cabin", "Snow Cabin"),
    effect(Kind::Scene, 0x35, "Sea Fire", "Sea Fire"),
    effect(Kind::Scene, 0x36, "Firefly Night", "Firefly Night"),
    effect(Kind::Scene, 0x37, "Game", "Game"),
    effect(Kind::Scene, 0x38, "Holiday", "Holiday"),
    effect(Kind::Scene, 0x39, "Working", "Working"),
    effect(Kind::Scene, 0x3A, "Party", "Party"),
    effect(Kind::Scene, 0x3B, "Fashion", "Fashion"),
    effect(Kind::Scene, 0x3C, "Sport", "Sport"),
    effect(Kind::Scene, 0x3D, "Christmas", "Christmas"),
    effect(Kind::Scene, 0x3E, "Valentine's Day", "Valentine's Day"),
    effect(Kind::Scene, 0x3F, "Halloween", "Halloween"),
    effect(Kind::Scene, 0x40, "Thanksgiving", "Thanksgiving"),
    effect(Kind::Scene, 0x41, "Mother's Day", "Mother's Day"),
    effect(Kind::Scene, 0x42, "Father's Day", "Father's Day"),
    effect(Kind::Scene, 0x43, "Summer Song", "Summer Song"),
    effect(Kind::Scene, 0x44, "Sea Dream", "Sea Dream"),
    effect(Kind::Scene, 0x45, "Love & Dream", "Love & Dream"),
    effect(Kind::Scene, 0x46, "Spring Fishing", "Spring Fishing"),
    effect(Kind::Scene, 0x47, "Neon World", "Neon World"),
    effect(Kind::Scene, 0x48, "Dream Mirror", "Dream Mirror"),
    // ── DIY patterns, 1..=16 ───────────────────────────────────────────────
    // Prefixed because the vendor reuses names across categories and Home
    // Assistant selects by name: "Meteor" alone would be ambiguous.
    effect(Kind::Diy, 0x01, "DIY Gradient", "Gradient"),
    effect(Kind::Diy, 0x02, "DIY Jump", "Jump"),
    effect(Kind::Diy, 0x03, "DIY Breath", "Breath"),
    effect(Kind::Diy, 0x04, "DIY Blink", "Blink"),
    effect(Kind::Diy, 0x05, "DIY Meteor", "Meteor"),
    effect(Kind::Diy, 0x06, "DIY Stack", "Stack"),
    effect(Kind::Diy, 0x07, "DIY Drift", "Drift"),
    effect(Kind::Diy, 0x08, "DIY Chase", "Chase"),
    effect(Kind::Diy, 0x09, "DIY Sway", "Sway"),
    effect(Kind::Diy, 0x0A, "DIY Flow", "Flow"),
    effect(Kind::Diy, 0x0B, "DIY Rainbow", "Rainbow"),
    effect(Kind::Diy, 0x0C, "DIY Flash", "Flash"),
    effect(Kind::Diy, 0x0D, "DIY Bounce", "Bounce"),
    effect(Kind::Diy, 0x0E, "DIY Shuttle", "Shuttle"),
    effect(Kind::Diy, 0x0F, "DIY Flicker", "Flicker"),
    effect(Kind::Diy, 0x10, "DIY Unfold", "Unfold"),
    // ── music modes, 0..=5 ─────────────────────────────────────────────────
    effect(Kind::Music, 0, "Music Energy", "Energy"),
    effect(Kind::Music, 1, "Music Breathing", "Breathing"),
    effect(Kind::Music, 2, "Music Beat", "Beat"),
    effect(Kind::Music, 3, "Music Meteor", "Meteor"),
    effect(Kind::Music, 4, "Music Starry Sky", "Starry Sky"),
    effect(Kind::Music, 5, "Music Neon", "Neon"),
];

/// Every effect, in the order they should be offered to a user.
#[must_use]
pub fn all() -> &'static [Effect] {
    EFFECTS
}

/// Look an effect up by its published name.
///
/// Exact match, deliberately: accepting a case-insensitive or whitespace-trimmed
/// near-miss would make a typo in a config file look like it worked.
#[must_use]
pub fn by_name(name: &str) -> Option<&'static Effect> {
    EFFECTS.iter().find(|effect| effect.name == name)
}

/// Look an effect up by category and identifier.
///
/// Needed to turn a bare `modeId` back into a name for the state payload — the
/// device does not echo what it is doing, so the daemon has to remember.
#[must_use]
pub fn by_id(kind: Kind, id: u8) -> Option<&'static Effect> {
    EFFECTS
        .iter()
        .find(|effect| effect.kind == kind && effect.id == id)
}

/// Only the entries of a given kind, in catalogue order.
pub fn of_kind(kind: Kind) -> impl Iterator<Item = &'static Effect> {
    EFFECTS.iter().filter(move |effect| effect.kind == kind)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn has_the_expected_number_of_each_kind() {
        assert_eq!(of_kind(Kind::Preset).count(), 8, "0xF8..=0xFF");
        assert_eq!(of_kind(Kind::Scene).count(), 24, "0x31..=0x48");
        assert_eq!(of_kind(Kind::Diy).count(), 16, "1..=16");
        assert_eq!(of_kind(Kind::Music).count(), 6, "0..=5");
        assert_eq!(
            of_kind(Kind::TwoWay).count(),
            3,
            "TV Screen, mirrored, and off"
        );
        assert_eq!(all().len(), 57, "the whole catalogue");
    }

    #[test]
    fn scene_and_preset_identifiers_are_contiguous_ranges() {
        // A gap would mean a transcription error: the vendor's ranges have none.
        let scenes: Vec<u8> = of_kind(Kind::Scene).map(|effect| effect.id).collect();
        assert_eq!(scenes, (0x31..=0x48).collect::<Vec<u8>>());

        let presets: Vec<u8> = of_kind(Kind::Preset).map(|effect| effect.id).collect();
        assert_eq!(presets, (0xF8..=0xFF).collect::<Vec<u8>>());

        let diy: Vec<u8> = of_kind(Kind::Diy).map(|effect| effect.id).collect();
        assert_eq!(diy, (1..=16).collect::<Vec<u8>>());

        let music: Vec<u8> = of_kind(Kind::Music).map(|effect| effect.id).collect();
        assert_eq!(music, (0..=5).collect::<Vec<u8>>());
    }

    #[test]
    fn published_names_are_unique() {
        // Home Assistant picks an effect by name. The vendor reuses several
        // names across categories, so this is the test that keeps the published
        // list unambiguous.
        let mut seen = HashSet::new();
        for effect in all() {
            assert!(
                seen.insert(effect.name),
                "duplicate published name {:?}",
                effect.name
            );
        }
    }

    #[test]
    fn the_three_meteors_are_distinguishable() {
        // The vendor calls one preset, one DIY pattern and one music mode
        // "Meteor". All three must appear, and each must be its own entry.
        let meteors: Vec<&Effect> = all()
            .iter()
            .filter(|effect| effect.vendor_name == "Meteor")
            .collect();
        assert_eq!(meteors.len(), 3);
        assert_eq!(
            meteors.iter().map(|effect| effect.kind).collect::<Vec<_>>(),
            [Kind::Preset, Kind::Diy, Kind::Music]
        );
        assert_eq!(
            meteors.iter().map(|e| e.id).collect::<Vec<_>>(),
            [0xFD, 5, 3]
        );
    }

    #[test]
    fn identifiers_are_unique_within_a_kind() {
        // The identifier is the identity on the wire, so two entries sharing one
        // inside a category would be indistinguishable to the device.
        //
        // Screen-sync is the exception that proves it: `TV Screen` and its
        // mirror deliberately share `0xF1` and are told apart by `direction`
        // alone, so the identity there is the pair rather than the identifier.
        for kind in [
            Kind::Preset,
            Kind::TwoWay,
            Kind::Scene,
            Kind::Diy,
            Kind::Music,
        ] {
            let mut seen = HashSet::new();
            for effect in of_kind(kind) {
                assert!(
                    seen.insert((effect.id, effect.direction)),
                    "{kind:?} repeats identifier {:#04X} with direction {}",
                    effect.id,
                    effect.direction
                );
            }
        }
    }

    #[test]
    fn only_screen_sync_carries_a_direction() {
        // Direction mirrors the animation for the two-way presets and is
        // ignored everywhere else, so anything non-two-way must leave it at zero
        // rather than carrying a stray value onto the wire.
        for effect in of_kind(Kind::TwoWay) {
            let _ = effect.direction;
        }
        for effect in all().iter().filter(|effect| effect.kind != Kind::TwoWay) {
            assert_eq!(
                effect.direction, 0,
                "{} should not carry a direction",
                effect.name
            );
        }
    }

    #[test]
    fn two_way_off_ignores_direction() {
        let off = by_name("Two-Way Off").expect("Two-Way Off must exist");
        assert_eq!(off.direction, 0);
        assert_eq!(off.id, 0xF2);
    }

    #[test]
    fn tv_screen_and_its_mirror_share_an_identifier_but_not_a_name() {
        let screen = by_name("TV Screen").expect("TV Screen must exist");
        let mirrored = by_name("TV Screen (mirrored)").expect("mirror must exist");
        assert_eq!(screen.id, 0xF1);
        assert_eq!(mirrored.id, 0xF1);
        assert_eq!(screen.direction, 0);
        assert_eq!(mirrored.direction, 1);
    }

    #[test]
    fn lookups_are_exact_and_unknown_names_return_nothing() {
        assert!(by_name("Superfluid").is_some());
        assert!(by_name("Icelandic Blue").is_some());
        // Case and whitespace matter: silently accepting a near-miss would make
        // a typo in a config file look like it worked.
        assert!(by_name("superfluid").is_none());
        assert!(by_name(" Superfluid").is_none());
        assert!(by_name("No Such Effect").is_none());
    }

    #[test]
    fn the_vendor_labels_survive_for_someone_reading_a_comment() {
        let superfluid = by_name("Superfluid").expect("Superfluid must exist");
        assert_eq!(superfluid.vendor_name, "Superfluid");
        assert_eq!(superfluid.kind, Kind::Preset);
        assert_eq!(superfluid.id, 0xFC);
    }
}
