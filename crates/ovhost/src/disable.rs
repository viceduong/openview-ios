//! Diagnostic kill-switches (`OPENVIEW_DISABLE` / hidden `--disable`): turn
//! individual optimisations off on a live host, so a viewer build can be
//! A/B tested against them without rebuilding it.
//!
//! The list is read once at startup ([`crate::settings::apply`]) into the
//! process-wide [`DISABLED`]. Viewer-negotiated features are switched off
//! centrally by masking their CAPS bits — both the bits the host advertises
//! (so the viewer does not send their commands) and the viewer's bits the
//! host acts on ([`Disabled::mask_caps`]); the rest are read where they act.

use std::fmt;
use std::sync::OnceLock;

use ovapp::terminal::{CAP_HEVC, CAP_LTR, CAP_PALETTE, CAP_VIEWPORT};

/// The environment variable read at startup (comma-separated,
/// case-insensitive feature names; `--disable` takes the same list).
pub(crate) const ENV: &str = "OPENVIEW_DISABLE";

/// One optimisation that can be switched off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Feature {
    /// HEVC video (`CAP_HEVC`): H.264 only.
    Hevc,
    /// LTR loss recovery (`CAP_LTR`): recovery by IDR.
    Ltr,
    /// Viewport priority (`CAP_VIEWPORT`): `VIEWPORT` ignored, no deferral.
    Viewport,
    /// Palette bands (`CAP_PALETTE`): plain zstd bands.
    Palette,
    /// Stable encode surface during motion: per-motion-rect encoder sizing.
    Stable,
    /// Input wake / full-rate boost after input.
    Boost,
    /// Adaptive video FEC: the fixed loss-driven ratio (10 % on a clean
    /// link, no 2 % floor).
    FecAdaptive,
}

impl Feature {
    /// Every feature, in log order.
    pub(crate) const ALL: [Self; 7] = [
        Self::Hevc,
        Self::Ltr,
        Self::Viewport,
        Self::Palette,
        Self::Stable,
        Self::Boost,
        Self::FecAdaptive,
    ];

    /// The name in `OPENVIEW_DISABLE` (lower case).
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Hevc => "hevc",
            Self::Ltr => "ltr",
            Self::Viewport => "viewport",
            Self::Palette => "palette",
            Self::Stable => "stable",
            Self::Boost => "boost",
            Self::FecAdaptive => "fec-adaptive",
        }
    }

    /// The feature called `name` (case-insensitive; `_` = `-`).
    pub(crate) fn from_name(name: &str) -> Option<Self> {
        let name = name.trim().to_ascii_lowercase().replace('_', "-");
        Self::ALL.into_iter().find(|f| f.name() == name)
    }

    /// The CAPS bit this feature is negotiated with (0 = host-only).
    fn cap(self) -> u32 {
        match self {
            Self::Hevc => CAP_HEVC,
            Self::Ltr => CAP_LTR,
            Self::Viewport => CAP_VIEWPORT,
            Self::Palette => CAP_PALETTE,
            Self::Stable | Self::Boost | Self::FecAdaptive => 0,
        }
    }

    fn bit(self) -> u8 {
        1 << self as u8
    }
}

/// A set of disabled features.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Disabled(u8);

impl Disabled {
    /// Nothing disabled.
    pub(crate) const NONE: Self = Self(0);

    /// Parse a comma-separated, case-insensitive list (`all` = every
    /// feature; blanks are skipped). Returns the set and the names that
    /// matched no feature.
    pub(crate) fn parse(list: &str) -> (Self, Vec<String>) {
        let mut set = Self::NONE;
        let mut unknown = Vec::new();
        for item in list.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            if item.eq_ignore_ascii_case("all") {
                Feature::ALL.into_iter().for_each(|f| set.insert(f));
            } else if let Some(f) = Feature::from_name(item) {
                set.insert(f);
            } else {
                unknown.push(item.to_owned());
            }
        }
        (set, unknown)
    }

    /// Disable `f`.
    pub(crate) fn insert(&mut self, f: Feature) {
        self.0 |= f.bit();
    }

    /// Whether `f` is disabled.
    pub(crate) fn has(self, f: Feature) -> bool {
        self.0 & f.bit() != 0
    }

    /// Nothing disabled.
    pub(crate) fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The CAPS bits of the disabled features.
    pub(crate) fn caps_mask(self) -> u32 {
        Feature::ALL
            .into_iter()
            .filter(|&f| self.has(f))
            .fold(0, |m, f| m | f.cap())
    }

    /// `caps` without the bits of the disabled features (applied to both
    /// the host's advertised CAPS and the viewer's).
    pub(crate) fn mask_caps(self, caps: u32) -> u32 {
        caps & !self.caps_mask()
    }
}

impl fmt::Display for Disabled {
    /// The disabled feature names, comma-separated (`none` when empty).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_empty() {
            return f.write_str("none");
        }
        let mut first = true;
        for feat in Feature::ALL.into_iter().filter(|&x| self.has(x)) {
            if !first {
                f.write_str(",")?;
            }
            f.write_str(feat.name())?;
            first = false;
        }
        Ok(())
    }
}

/// The process-wide set, stored once by [`init`] (unset = nothing
/// disabled, e.g. in unit tests).
static DISABLED: OnceLock<Disabled> = OnceLock::new();

/// Store the startup list (`--disable`, else `OPENVIEW_DISABLE`) and log
/// it: one INFO line when something is disabled, a warning per unknown
/// name. Later calls are ignored.
pub(crate) fn init(list: Option<&str>) {
    let (set, unknown) = Disabled::parse(list.unwrap_or(""));
    for name in &unknown {
        tracing::warn!(
            "{ENV}: unknown feature `{name}` ignored (known: {}, all)",
            Disabled::parse("all").0
        );
    }
    if !set.is_empty() {
        tracing::info!("diagnostics: disabled features: {set}");
    }
    let _ = DISABLED.set(set);
}

/// The disabled features.
pub(crate) fn get() -> Disabled {
    DISABLED.get().copied().unwrap_or_default()
}

/// Whether `f` is disabled for this process.
pub(crate) fn has(f: Feature) -> bool {
    get().has(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_comma_separated_case_insensitive() {
        let (set, unknown) = Disabled::parse(" HEVC, ltr ,Viewport,,fec_adaptive");
        assert!(unknown.is_empty());
        assert!(set.has(Feature::Hevc));
        assert!(set.has(Feature::Ltr));
        assert!(set.has(Feature::Viewport));
        assert!(set.has(Feature::FecAdaptive));
        assert!(!set.has(Feature::Palette));
        assert!(!set.has(Feature::Stable));
        assert!(!set.has(Feature::Boost));
        assert_eq!(set.to_string(), "hevc,ltr,viewport,fec-adaptive");
    }

    #[test]
    fn empty_list_disables_nothing() {
        for s in ["", " ", ",,", " , "] {
            let (set, unknown) = Disabled::parse(s);
            assert_eq!(set, Disabled::NONE, "{s:?}");
            assert!(unknown.is_empty());
        }
        assert_eq!(Disabled::NONE.to_string(), "none");
    }

    #[test]
    fn unknown_names_are_reported_not_fatal() {
        let (set, unknown) = Disabled::parse("boost,turbo,Stable,h265");
        assert!(set.has(Feature::Boost) && set.has(Feature::Stable));
        assert_eq!(unknown, ["turbo", "h265"]);
    }

    #[test]
    fn all_disables_every_feature() {
        let (set, _) = Disabled::parse("ALL");
        assert!(Feature::ALL.into_iter().all(|f| set.has(f)));
        assert_eq!(
            set.to_string(),
            "hevc,ltr,viewport,palette,stable,boost,fec-adaptive"
        );
    }

    #[test]
    fn every_name_round_trips() {
        for f in Feature::ALL {
            assert_eq!(Feature::from_name(f.name()), Some(f));
            assert_eq!(Feature::from_name(&f.name().to_uppercase()), Some(f));
        }
    }

    #[test]
    fn caps_mask_covers_negotiated_features_only() {
        let all = ovapp::terminal::LOCAL_CAPS | ovapp::terminal::CAP_H264;
        assert_eq!(Disabled::NONE.mask_caps(all), all);

        let (set, _) = Disabled::parse("hevc,ltr,viewport,palette");
        assert_eq!(
            set.caps_mask(),
            CAP_HEVC | CAP_LTR | CAP_VIEWPORT | CAP_PALETTE
        );
        let masked = set.mask_caps(all | CAP_HEVC);
        assert_eq!(
            masked & (CAP_HEVC | CAP_LTR | CAP_VIEWPORT | CAP_PALETTE),
            0
        );
        // Everything else survives (tiles, H.264, ...).
        assert_eq!(masked, (all | CAP_HEVC) & !set.caps_mask());
        assert_ne!(masked & ovapp::terminal::CAP_H264, 0);
        assert_ne!(masked & ovapp::terminal::CAP_TILES, 0);

        // Host-only switches leave CAPS alone.
        let (host_only, _) = Disabled::parse("stable,boost,fec-adaptive");
        assert_eq!(host_only.caps_mask(), 0);
        assert_eq!(host_only.mask_caps(all), all);

        // One switch masks exactly its bit.
        let (hevc, _) = Disabled::parse("hevc");
        assert_eq!(hevc.mask_caps(all | CAP_HEVC), all & !CAP_HEVC);
    }
}
