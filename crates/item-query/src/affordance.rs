//! G4 size and container-affordance evidence.
//!
//! Priors are configuration, not observations. Built-in class priors cover the
//! default household targets and the G2 cover vocabulary; an optional TOML
//! file (`item-query --priors`, see `priors.example.toml`) adds measured
//! containers and overrides object sizes. Nothing here is written to the
//! database, and a class prior alone never promotes a candidate to
//! containment: that needs a container whose opening and usable interior were
//! recorded in the priors file.

use std::path::Path;
use std::sync::LazyLock;

use anyhow::{Context as _, bail};
use serde::Deserialize;

pub const BUILTIN_PRIORS_REF: &str = "builtin:g4-priors-1";

/// A closed interval in metres.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(try_from = "[f32; 2]")]
pub struct Interval {
    pub min: f32,
    pub max: f32,
}

impl TryFrom<[f32; 2]> for Interval {
    type Error = String;

    fn try_from([min, max]: [f32; 2]) -> Result<Self, Self::Error> {
        if !min.is_finite() || !max.is_finite() || min < 0.0 || max <= 0.0 || min > max {
            return Err(format!(
                "[{min}, {max}] is not an interval in metres (need 0 <= min <= max, max > 0)"
            ));
        }
        Ok(Self { min, max })
    }
}

/// Three extents of a rigid object or a usable interior. The axes carry no
/// orientation: the fit check tries every axis-aligned assignment.
pub type Extents = [Option<Interval>; 3];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriorSource {
    /// Coarse class knowledge shipped with item-query (high uncertainty).
    Builtin,
    /// Entered by a person in the priors file.
    File,
}

impl PriorSource {
    fn describe(self) -> &'static str {
        match self {
            Self::Builtin => "class prior",
            Self::File => "priors file",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Has a usable interior or opening: box, drawer, bag, cabinet, basket.
    Container,
    /// Can hide an object without holding it: book, lid, cloth, tray.
    Cover,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpeningState {
    Open,
    Closed,
    Unclear,
    NotApplicable,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ObjectPrior {
    pub label: String,
    pub extents: Extents,
    pub source: PriorSource,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContainerPrior {
    pub label: String,
    pub camera: Option<String>,
    pub zone: Option<String>,
    pub role: Role,
    pub opening_state: OpeningState,
    pub interior: Extents,
    pub source: PriorSource,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Priors {
    objects: Vec<ObjectPrior>,
    containers: Vec<ContainerPrior>,
    reference: String,
}

static BUILTIN: LazyLock<Priors> = LazyLock::new(Priors::build_builtin);

impl Priors {
    pub fn builtin() -> &'static Self {
        &BUILTIN
    }

    /// Built-in priors overlaid with a priors file.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let bytes = std::fs::read(path)
            .with_context(|| format!("reading priors file {}", path.display()))?;
        let text = std::str::from_utf8(&bytes)
            .with_context(|| format!("priors file {} is not UTF-8", path.display()))?;
        let reference = format!(
            "{BUILTIN_PRIORS_REF}+file:{}#fnv1a64:{:016x}",
            path.display(),
            fnv1a64(&bytes)
        );
        Self::from_toml_str(text, reference)
            .with_context(|| format!("priors file {}", path.display()))
    }

    pub fn from_toml_str(text: &str, reference: String) -> anyhow::Result<Self> {
        let raw: RawFile = toml::from_str(text)?;
        let mut priors = Self::builtin().clone();
        priors.reference = reference;

        let mut file_objects: Vec<ObjectPrior> = Vec::new();
        for object in raw.object {
            let label = checked_label(&object.label)?;
            if file_objects.iter().any(|seen| seen.label == label) {
                bail!("[[object]] label '{label}' is listed twice");
            }
            file_objects.push(ObjectPrior {
                label,
                extents: [object.length_m, object.width_m, object.height_m],
                source: PriorSource::File,
            });
        }

        let mut file_containers: Vec<ContainerPrior> = Vec::new();
        for container in raw.container {
            let label = checked_label(&container.label)?;
            if is_person_label(&label) {
                bail!("[[container]] '{label}': a person can only occlude, never cover or contain");
            }
            if container.zone.is_some() && container.camera.is_none() {
                bail!(
                    "[[container]] '{label}': zone names are per camera, so `zone` needs `camera`"
                );
            }
            let interior = [
                container.interior_length_m,
                container.interior_width_m,
                container.interior_height_m,
            ];
            let opening_state = match (container.role, container.opening_state) {
                (Role::Cover, None | Some(OpeningState::NotApplicable)) => {
                    OpeningState::NotApplicable
                }
                (Role::Cover, Some(state)) => {
                    bail!("[[container]] '{label}': a cover has no opening (got {state:?})")
                }
                (Role::Container, None) => OpeningState::Unclear,
                (Role::Container, Some(OpeningState::NotApplicable)) => {
                    bail!(
                        "[[container]] '{label}': a container's opening_state cannot be not_applicable"
                    )
                }
                (Role::Container, Some(state)) => state,
            };
            if container.role == Role::Cover && interior.iter().any(Option::is_some) {
                bail!("[[container]] '{label}': a cover has no usable interior");
            }
            let entry = ContainerPrior {
                label,
                camera: container.camera,
                zone: container.zone,
                role: container.role,
                opening_state,
                interior,
                source: PriorSource::File,
            };
            if file_containers.iter().any(|seen| {
                seen.label == entry.label && seen.camera == entry.camera && seen.zone == entry.zone
            }) {
                bail!(
                    "[[container]] '{}' is listed twice for the same camera/zone scope",
                    entry.label
                );
            }
            file_containers.push(entry);
        }

        priors
            .objects
            .retain(|builtin| !file_objects.iter().any(|f| f.label == builtin.label));
        priors.objects.extend(file_objects);
        // File entries go first so a label-only file entry shadows the
        // built-in entry of the same label.
        file_containers.extend(priors.containers);
        priors.containers = file_containers;
        Ok(priors)
    }

    pub fn reference(&self) -> &str {
        &self.reference
    }

    pub fn object(&self, label: &str) -> Option<&ObjectPrior> {
        let label = label.to_ascii_lowercase();
        self.objects.iter().find(|object| object.label == label)
    }

    /// The most specific entry for this label: camera+zone, then camera,
    /// then label only (file before built-in).
    pub fn container(&self, label: &str, camera: &str, zone: &str) -> Option<&ContainerPrior> {
        let label = label.to_ascii_lowercase();
        self.containers
            .iter()
            .filter(|entry| {
                entry.label == label
                    && entry.camera.as_deref().is_none_or(|c| c == camera)
                    && entry.zone.as_deref().is_none_or(|z| z == zone)
            })
            .max_by_key(|entry| {
                // `max_by_key` keeps the last maximum; reverse the order so the
                // first (file) entry wins among equally specific ones.
                (
                    entry.camera.is_some() as u8 + entry.zone.is_some() as u8,
                    std::cmp::Reverse(entry.source == PriorSource::Builtin),
                )
            })
    }

    fn build_builtin() -> Self {
        let iv = |min: f32, max: f32| Some(Interval { min, max });
        // Deliberately wide: a wide interval makes both "fits" and "does not
        // fit" harder to reach, so a class prior errs toward unknown.
        let objects = [
            ("keys", iv(0.05, 0.12), iv(0.02, 0.08), iv(0.002, 0.04)),
            ("remote", iv(0.15, 0.25), iv(0.035, 0.07), iv(0.015, 0.035)),
            ("phone", iv(0.12, 0.18), iv(0.06, 0.09), iv(0.006, 0.015)),
            (
                "cell phone",
                iv(0.12, 0.18),
                iv(0.06, 0.09),
                iv(0.006, 0.015),
            ),
            ("wallet", iv(0.08, 0.20), iv(0.06, 0.11), iv(0.005, 0.03)),
            ("glasses", iv(0.12, 0.16), iv(0.035, 0.06), iv(0.02, 0.05)),
            ("scissors", iv(0.10, 0.25), iv(0.04, 0.10), iv(0.005, 0.02)),
            ("charger", iv(0.04, 0.12), iv(0.03, 0.08), iv(0.02, 0.04)),
            ("medicine", iv(0.05, 0.15), iv(0.03, 0.10), iv(0.005, 0.05)),
            ("bottle", iv(0.15, 0.35), iv(0.05, 0.10), iv(0.05, 0.10)),
            ("cup", iv(0.07, 0.15), iv(0.06, 0.12), iv(0.06, 0.12)),
            ("laptop", iv(0.28, 0.42), iv(0.19, 0.30), iv(0.012, 0.04)),
            ("umbrella", iv(0.20, 1.00), iv(0.04, 0.12), iv(0.04, 0.12)),
        ]
        .into_iter()
        .map(|(label, length, width, height)| ObjectPrior {
            label: label.into(),
            extents: [length, width, height],
            source: PriorSource::Builtin,
        })
        .collect();

        // Built-in containers never carry an interior or a known opening:
        // a class says nothing about whether this particular box is a ring
        // box or a moving box, or whether its lid is off.
        let containers = [
            ("box", Role::Container),
            ("drawer", Role::Container),
            ("bag", Role::Container),
            ("cabinet", Role::Container),
            ("basket", Role::Container),
            ("book", Role::Cover),
            ("tray", Role::Cover),
            ("cloth", Role::Cover),
            ("lid", Role::Cover),
        ]
        .into_iter()
        .map(|(label, role)| ContainerPrior {
            label: label.into(),
            camera: None,
            zone: None,
            role,
            opening_state: match role {
                Role::Container => OpeningState::Unclear,
                Role::Cover => OpeningState::NotApplicable,
            },
            interior: [None; 3],
            source: PriorSource::Builtin,
        })
        .collect();

        Self {
            objects,
            containers,
            reference: BUILTIN_PRIORS_REF.into(),
        }
    }
}

pub fn is_person_label(label: &str) -> bool {
    matches!(
        label.to_ascii_lowercase().as_str(),
        "person" | "human" | "hand" | "people"
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fit {
    Fits,
    NotFits,
    Unknown,
}

/// Interval fit of a rigid object in a usable interior (design §7.3).
///
/// `Fits` when the object at its largest fits the interior at its smallest in
/// some axis-aligned orientation. `NotFits` only when one of the object's
/// extents is certainly longer than the interior's space diagonal, so no
/// orientation (diagonal included) can work. Everything else is `Unknown`:
/// no packing is attempted.
pub fn fit(object: &Extents, interior: &Extents) -> Fit {
    let longest_min = object.iter().flatten().map(|i| i.min).reduce(f32::max);
    if let (Some(longest_min), Some(interior)) = (longest_min, all_three(interior)) {
        let diagonal = interior.iter().map(|i| i.max * i.max).sum::<f32>().sqrt();
        if longest_min > diagonal {
            return Fit::NotFits;
        }
    }
    let (Some(object), Some(interior)) = (all_three(object), all_three(interior)) else {
        return Fit::Unknown;
    };
    let mut needed = object.map(|i| i.max);
    let mut available = interior.map(|i| i.min);
    needed.sort_by(|a, b| b.total_cmp(a));
    available.sort_by(|a, b| b.total_cmp(a));
    if needed.iter().zip(&available).all(|(n, a)| n <= a) {
        Fit::Fits
    } else {
        Fit::Unknown
    }
}

fn all_three(extents: &Extents) -> Option<[Interval; 3]> {
    Some([extents[0]?, extents[1]?, extents[2]?])
}

#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    pub state: &'static str,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Assessment {
    pub affordance: Finding,
    /// Only containers get a size finding; a cover holds nothing.
    pub size: Option<Finding>,
    /// Open container, recorded in the priors file, whose interior fits the
    /// target at its largest prior size.
    pub promotable: bool,
}

pub fn assess(
    target_label: &str,
    object: Option<&ObjectPrior>,
    container: &ContainerPrior,
) -> Assessment {
    let cover = &container.label;
    if container.role == Role::Cover {
        return Assessment {
            affordance: Finding {
                state: "conflicting",
                detail: format!(
                    "{cover} is a cover ({}), not a container: containment is not considered",
                    container.source.describe()
                ),
            },
            size: None,
            promotable: false,
        };
    }

    let affordance = match (container.opening_state, container.source) {
        (OpeningState::Open, PriorSource::File) => Finding {
            state: "supporting",
            detail: format!("{cover} is recorded as an open container (priors file)"),
        },
        (OpeningState::Closed, _) => Finding {
            state: "unknown",
            detail: format!(
                "{cover} is recorded as closed; an item placed inside before it closed can be neither ruled in nor out"
            ),
        },
        _ => Finding {
            state: "unknown",
            detail: format!(
                "{cover} is a container ({}), but whether it is open is not recorded",
                container.source.describe()
            ),
        },
    };

    let has_interior = container.interior.iter().any(Option::is_some);
    let outcome = object.map(|object| fit(&object.extents, &container.interior));
    let size = match (object, outcome) {
        _ if !has_interior => Finding {
            state: "unknown",
            detail: format!("the usable interior of {cover} is not recorded"),
        },
        (None, _) | (_, None) => Finding {
            state: "unknown",
            detail: format!("no size prior for {target_label}"),
        },
        (Some(object), Some(Fit::Fits)) => Finding {
            state: "supporting",
            detail: format!(
                "{target_label} at its largest ({}, {}) fits the recorded interior of {cover} at its smallest ({}) in some axis-aligned orientation; the opening size was not checked",
                format_extents(&object.extents, |i| i.max),
                object.source.describe(),
                format_extents(&container.interior, |i| i.min),
            ),
        },
        (Some(object), Some(Fit::NotFits)) => Finding {
            state: "conflicting",
            detail: format!(
                "{target_label} is at least {:.3} m long ({}), longer than the {cover} interior's diagonal ({:.3} m)",
                object
                    .extents
                    .iter()
                    .flatten()
                    .map(|i| i.min)
                    .fold(0.0, f32::max),
                object.source.describe(),
                container
                    .interior
                    .iter()
                    .flatten()
                    .map(|i| i.max * i.max)
                    .sum::<f32>()
                    .sqrt(),
            ),
        },
        (Some(object), Some(Fit::Unknown)) => Finding {
            state: "unknown",
            detail: format!(
                "{target_label} ({}, {}) and the {cover} interior ({}) overlap as intervals; fit is undecided",
                format_extents(&object.extents, |i| i.max),
                object.source.describe(),
                format_extents(&container.interior, |i| i.min),
            ),
        },
    };

    let promotable = container.source == PriorSource::File
        && container.opening_state == OpeningState::Open
        && outcome == Some(Fit::Fits);
    Assessment {
        affordance,
        size: Some(size),
        promotable,
    }
}

fn format_extents(extents: &Extents, pick: impl Fn(&Interval) -> f32) -> String {
    let parts: Vec<String> = extents
        .iter()
        .map(|extent| {
            extent
                .as_ref()
                .map_or("?".into(), |i| format!("{:.3}", pick(i)))
        })
        .collect();
    format!("{} m", parts.join(" x "))
}

fn checked_label(label: &str) -> anyhow::Result<String> {
    let label = label.trim().to_ascii_lowercase();
    if label.is_empty() {
        bail!("an entry has an empty label");
    }
    Ok(label)
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    #[serde(default)]
    object: Vec<RawObject>,
    #[serde(default)]
    container: Vec<RawContainer>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawObject {
    label: String,
    length_m: Option<Interval>,
    width_m: Option<Interval>,
    height_m: Option<Interval>,
    #[allow(dead_code)]
    note: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawContainer {
    label: String,
    camera: Option<String>,
    zone: Option<String>,
    role: Role,
    opening_state: Option<OpeningState>,
    interior_length_m: Option<Interval>,
    interior_width_m: Option<Interval>,
    interior_height_m: Option<Interval>,
    #[allow(dead_code)]
    note: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn iv(min: f32, max: f32) -> Option<Interval> {
        Some(Interval { min, max })
    }

    fn parse(text: &str) -> anyhow::Result<Priors> {
        Priors::from_toml_str(text, "test".into())
    }

    #[test]
    fn the_shipped_example_parses_and_records_an_open_box() {
        let priors = parse(include_str!("../priors.example.toml")).unwrap();
        let shoebox = priors.container("box", "desk", "desk").unwrap();
        assert_eq!(shoebox.source, PriorSource::File);
        assert_eq!(shoebox.opening_state, OpeningState::Open);
        assert!(shoebox.interior.iter().all(Option::is_some));
    }

    #[test]
    fn builtin_cover_vocabulary_matches_the_g2_list() {
        let priors = Priors::builtin();
        for label in [
            "box", "book", "bag", "drawer", "cabinet", "basket", "tray", "cloth", "lid",
        ] {
            assert!(priors.container(label, "any", "any").is_some(), "{label}");
        }
        assert!(priors.container("chair", "any", "any").is_none());
        assert!(priors.container("person", "any", "any").is_none());
    }

    #[test]
    fn builtin_containers_never_carry_an_interior_or_an_open_state() {
        for entry in &Priors::builtin().containers {
            assert_eq!(entry.interior, [None; 3], "{}", entry.label);
            assert_ne!(entry.opening_state, OpeningState::Open, "{}", entry.label);
        }
    }

    #[test]
    fn intervals_reject_inverted_negative_and_zero_ranges() {
        for bad in ["[0.3, 0.2]", "[-0.1, 0.2]", "[0.0, 0.0]"] {
            let text = format!("[[object]]\nlabel = \"keys\"\nlength_m = {bad}\n");
            assert!(parse(&text).is_err(), "{bad} should be rejected");
        }
        assert!(parse("[[object]]\nlabel = \"keys\"\nlength_m = [0, 1]\n").is_ok());
    }

    #[test]
    fn typos_and_inconsistent_entries_are_errors_not_silent_defaults() {
        let typo = "[[container]]\nlabel = \"box\"\nrole = \"container\"\ninterior_lenght_m = [0.1, 0.2]\n";
        assert!(format!("{:#}", parse(typo).unwrap_err()).contains("interior_lenght_m"));

        let cases = [
            "[[container]]\nlabel = \"person\"\nrole = \"cover\"\n",
            "[[container]]\nlabel = \"box\"\nzone = \"desk\"\nrole = \"container\"\n",
            "[[container]]\nlabel = \"book\"\nrole = \"cover\"\nopening_state = \"open\"\n",
            "[[container]]\nlabel = \"book\"\nrole = \"cover\"\ninterior_length_m = [0.1, 0.2]\n",
            "[[container]]\nlabel = \"box\"\nrole = \"container\"\nopening_state = \"not_applicable\"\n",
            "[[container]]\nlabel = \"box\"\nrole = \"container\"\n[[container]]\nlabel = \"BOX\"\nrole = \"container\"\n",
            "[[object]]\nlabel = \" \"\n",
        ];
        for text in cases {
            assert!(parse(text).is_err(), "should be rejected:\n{text}");
        }
    }

    #[test]
    fn the_most_specific_container_entry_wins_and_file_shadows_builtin() {
        let priors = parse(
            "[[container]]\nlabel = \"box\"\nrole = \"container\"\nopening_state = \"closed\"\n\
             [[container]]\nlabel = \"box\"\ncamera = \"desk\"\nrole = \"container\"\nopening_state = \"open\"\n\
             [[container]]\nlabel = \"box\"\ncamera = \"desk\"\nzone = \"shelf\"\nrole = \"cover\"\n",
        )
        .unwrap();
        assert_eq!(
            priors.container("Box", "desk", "shelf").unwrap().role,
            Role::Cover
        );
        assert_eq!(
            priors
                .container("box", "desk", "floor")
                .unwrap()
                .opening_state,
            OpeningState::Open
        );
        let elsewhere = priors.container("box", "hall", "floor").unwrap();
        assert_eq!(elsewhere.source, PriorSource::File);
        assert_eq!(elsewhere.opening_state, OpeningState::Closed);
        assert_eq!(
            priors.container("drawer", "desk", "floor").unwrap().source,
            PriorSource::Builtin
        );
    }

    #[test]
    fn a_file_object_replaces_the_builtin_prior_whole() {
        let priors = parse("[[object]]\nlabel = \"keys\"\nlength_m = [0.04, 0.06]\n").unwrap();
        let keys = priors.object("KEYS").unwrap();
        assert_eq!(keys.source, PriorSource::File);
        assert_eq!(keys.extents, [iv(0.04, 0.06), None, None]);
        assert_eq!(
            priors.object("remote").unwrap().source,
            PriorSource::Builtin
        );
    }

    #[test]
    fn fit_is_interval_logic_over_any_axis_aligned_orientation() {
        let keys = [iv(0.05, 0.12), iv(0.02, 0.08), iv(0.002, 0.04)];
        // Axes listed in another order still fit: orientation is free.
        let shoebox = [iv(0.08, 0.10), iv(0.28, 0.30), iv(0.15, 0.18)];
        assert_eq!(fit(&keys, &shoebox), Fit::Fits);

        // The box's smallest interior is below the keys' largest size: undecided.
        let tight = [iv(0.10, 0.20), iv(0.07, 0.10), iv(0.03, 0.05)];
        assert_eq!(fit(&keys, &tight), Fit::Unknown);

        // A 0.20 m minimum length exceeds a ~0.087 m diagonal: no orientation fits.
        let umbrella = [iv(0.20, 1.00), iv(0.04, 0.12), iv(0.04, 0.12)];
        let ring_box = [iv(0.04, 0.05), iv(0.04, 0.05), iv(0.03, 0.05)];
        assert_eq!(fit(&umbrella, &ring_box), Fit::NotFits);

        // Longer than every axis but shorter than the diagonal: may fit
        // diagonally, so it must stay unknown rather than rejected.
        let pencil = [iv(0.11, 0.11), iv(0.007, 0.007), iv(0.007, 0.007)];
        let cube = [iv(0.10, 0.10), iv(0.10, 0.10), iv(0.10, 0.10)];
        assert_eq!(fit(&pencil, &cube), Fit::Unknown);

        // Missing axes on either side never produce `Fits`.
        let partial = [iv(0.25, 0.30), None, iv(0.08, 0.10)];
        assert_eq!(fit(&keys, &partial), Fit::Unknown);
        assert_eq!(fit(&[iv(0.05, 0.12), None, None], &shoebox), Fit::Unknown);
        // ...but a known length can still rule a small interior out.
        assert_eq!(fit(&[iv(0.20, 1.00), None, None], &ring_box), Fit::NotFits);
    }

    fn container(
        source: PriorSource,
        opening_state: OpeningState,
        interior: Extents,
    ) -> ContainerPrior {
        ContainerPrior {
            label: "box".into(),
            camera: None,
            zone: None,
            role: Role::Container,
            opening_state,
            interior,
            source,
        }
    }

    #[test]
    fn only_an_open_measured_file_container_that_fits_is_promotable() {
        let keys = Priors::builtin().object("keys").unwrap();
        let shoebox = [iv(0.25, 0.30), iv(0.15, 0.18), iv(0.08, 0.10)];

        let measured = assess(
            "keys",
            Some(keys),
            &container(PriorSource::File, OpeningState::Open, shoebox),
        );
        assert!(measured.promotable);
        assert_eq!(measured.affordance.state, "supporting");
        assert_eq!(measured.size.as_ref().unwrap().state, "supporting");

        let closed = assess(
            "keys",
            Some(keys),
            &container(PriorSource::File, OpeningState::Closed, shoebox),
        );
        assert!(!closed.promotable);
        assert_eq!(closed.affordance.state, "unknown");

        let unclear = assess(
            "keys",
            Some(keys),
            &container(PriorSource::File, OpeningState::Unclear, shoebox),
        );
        assert!(!unclear.promotable);

        let no_interior = assess(
            "keys",
            Some(keys),
            &container(PriorSource::File, OpeningState::Open, [None; 3]),
        );
        assert!(!no_interior.promotable);
        assert_eq!(no_interior.size.as_ref().unwrap().state, "unknown");

        let unknown_object = assess(
            "gizmo",
            None,
            &container(PriorSource::File, OpeningState::Open, shoebox),
        );
        assert!(!unknown_object.promotable);
        assert!(
            unknown_object
                .size
                .unwrap()
                .detail
                .contains("no size prior")
        );

        // A class-prior container is never promotable, even if it somehow
        // carried an open state and an interior.
        let builtin = assess(
            "keys",
            Some(keys),
            &container(PriorSource::Builtin, OpeningState::Open, shoebox),
        );
        assert!(!builtin.promotable);
        assert_eq!(builtin.affordance.state, "unknown");
    }

    #[test]
    fn a_too_small_container_conflicts_and_a_cover_never_contains() {
        let umbrella = Priors::builtin().object("umbrella").unwrap();
        let ring_box = [iv(0.04, 0.05), iv(0.04, 0.05), iv(0.03, 0.05)];
        let small = assess(
            "umbrella",
            Some(umbrella),
            &container(PriorSource::File, OpeningState::Open, ring_box),
        );
        assert!(!small.promotable);
        assert_eq!(small.size.unwrap().state, "conflicting");

        let book = Priors::builtin().container("book", "cam", "desk").unwrap();
        let cover = assess("keys", Priors::builtin().object("keys"), book);
        assert!(!cover.promotable);
        assert_eq!(cover.affordance.state, "conflicting");
        assert!(cover.size.is_none());
    }

    #[test]
    fn the_priors_reference_changes_with_the_file_contents() {
        assert_ne!(fnv1a64(b"a"), fnv1a64(b"b"));
        assert_eq!(Priors::builtin().reference(), BUILTIN_PRIORS_REF);
    }
}
