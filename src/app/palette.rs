//! The named colours an alert can use.
//!
//! A closed list on purpose. An alert is something you wire into an automation
//! at two in the morning and want to get right the first time, so the names are
//! ordinary English words rather than a hue in degrees: `alert/red` is a thing
//! you can type without a reference, and a typo is rejected at startup instead
//! of flashing the wrong colour.
//!
//! The list is also what the discovery document iterates over, which is how a
//! colour that was switched off can have its button withdrawn cleanly.

/// One named colour, in the terms the strip understands.
///
/// Hue is degrees around the wheel and saturation is percent, which is exactly
/// what [`crate::protocol::command::colour`] takes — so nothing here has to be
/// converted, and there is no second opinion about what "purple" means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NamedColour {
    pub name: &'static str,
    pub hue: u16,
    pub saturation: u8,
}

/// Every colour an alert can be asked for, in the order a person would list them.
///
/// Ten, not a hundred: these are signals, and a signal that needs a reference
/// chart is not a signal.
pub const ALL: [NamedColour; 10] = [
    NamedColour {
        name: "red",
        hue: 0,
        saturation: 100,
    },
    NamedColour {
        name: "orange",
        hue: 30,
        saturation: 100,
    },
    NamedColour {
        name: "yellow",
        hue: 60,
        saturation: 100,
    },
    NamedColour {
        name: "lime",
        hue: 90,
        saturation: 100,
    },
    NamedColour {
        name: "green",
        hue: 120,
        saturation: 100,
    },
    NamedColour {
        name: "cyan",
        hue: 180,
        saturation: 100,
    },
    NamedColour {
        name: "blue",
        hue: 240,
        saturation: 100,
    },
    NamedColour {
        name: "purple",
        hue: 280,
        saturation: 100,
    },
    NamedColour {
        name: "pink",
        hue: 330,
        saturation: 100,
    },
    NamedColour {
        name: "white",
        hue: 0,
        saturation: 0,
    },
];

/// What an alert does when it was not given a colour it recognises.
///
/// Red, because an unknown identifier should still fire: a doorbell that does
/// nothing because of a typo is worse than one flashing the wrong colour.
pub const FALLBACK: NamedColour = ALL[0];

/// Look a colour up by name, case-insensitively.
#[must_use]
pub fn by_name(name: &str) -> Option<NamedColour> {
    ALL.iter()
        .copied()
        .find(|colour| colour.name.eq_ignore_ascii_case(name))
}

/// Every name, for error messages that tell the operator what to type instead.
#[must_use]
pub fn names() -> String {
    ALL.iter()
        .map(|colour| colour.name)
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_name_is_unique_and_lowercase() {
        // The names go into MQTT topics and into Home Assistant's entity ids, so
        // a capital or a duplicate would produce either an awkward topic or a
        // collision between two buttons.
        let mut seen = Vec::new();
        for colour in ALL {
            assert_eq!(colour.name, colour.name.to_lowercase());
            assert!(
                !seen.contains(&colour.name),
                "duplicate colour {:?}",
                colour.name
            );
            seen.push(colour.name);
        }
    }

    #[test]
    fn every_colour_fits_the_protocol_limits() {
        // The builder rejects anything outside these ranges, and a rejected frame
        // would mean an alert that silently does nothing.
        for colour in ALL {
            assert!(
                crate::protocol::command::colour(colour.hue, colour.saturation, 1000).is_ok(),
                "{:?} cannot be built",
                colour.name
            );
        }
    }

    #[test]
    fn lookup_ignores_case_and_rejects_nonsense() {
        assert_eq!(by_name("RED"), Some(ALL[0]));
        assert_eq!(
            by_name(" Blue "),
            None,
            "callers trim; the palette does not"
        );
        assert_eq!(by_name("burgundy"), None);
    }

    #[test]
    fn the_fallback_is_red() {
        assert_eq!(FALLBACK.name, "red");
    }

    #[test]
    fn white_is_white_and_not_just_hue_zero() {
        // Saturation is what separates white from red, and getting it wrong here
        // turns "flash white" into "flash red".
        let white = by_name("white").expect("in the palette");
        assert_eq!(white.saturation, 0);
    }
}
