/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use anyhow::Context;
use anyhow::bail;
use semver::Version;
use semver::VersionReq;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct VersionReqBounds {
    lower: Option<VersionBound>,
    upper: Option<VersionBound>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct CompatibilityLane {
    lower: Version,
    upper: Version,
}

impl VersionReqBounds {
    fn empty() -> Self {
        Self {
            lower: None,
            upper: None,
        }
    }

    fn lower(bound: VersionBound) -> Self {
        Self {
            lower: Some(bound),
            upper: None,
        }
    }

    fn upper(bound: VersionBound) -> Self {
        Self {
            lower: None,
            upper: Some(bound),
        }
    }

    fn range(lower: VersionBound, upper: VersionBound) -> Self {
        Self {
            lower: Some(lower),
            upper: Some(upper),
        }
    }

    fn exact(version: Version) -> Self {
        Self::range(
            VersionBound::inclusive(version.clone()),
            VersionBound::inclusive(version),
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct VersionBound {
    version: Version,
    inclusive: bool,
}

impl VersionBound {
    fn new(version: Version, inclusive: bool) -> Self {
        Self { version, inclusive }
    }

    fn inclusive(version: Version) -> Self {
        Self::new(version, true)
    }

    fn exclusive(version: Version) -> Self {
        Self::new(version, false)
    }
}

pub(crate) fn version_req_bounds(req: &VersionReq) -> anyhow::Result<VersionReqBounds> {
    let mut bounds = VersionReqBounds::empty();
    for comparator in &req.comparators {
        let comparator_bounds = comparator_bounds(comparator)?;
        merge_lower_bound(&mut bounds.lower, comparator_bounds.lower);
        merge_upper_bound(&mut bounds.upper, comparator_bounds.upper);
    }
    Ok(bounds)
}

pub(crate) fn version_bounds_subset(
    narrowed: &VersionReqBounds,
    original: &VersionReqBounds,
) -> bool {
    lower_bound_subset(&narrowed.lower, &original.lower)
        && upper_bound_subset(&narrowed.upper, &original.upper)
}

pub(crate) fn version_req_to_compatibility_lane(
    req: &VersionReq,
    version: &Version,
) -> anyhow::Result<VersionReq> {
    let mut bounds = version_req_bounds(req)?;
    let lane_bounds = compatibility_lane_bounds(version)?;
    merge_lower_bound(&mut bounds.lower, lane_bounds.lower);
    merge_upper_bound(&mut bounds.upper, lane_bounds.upper);
    let req = format_version_req_bounds(&bounds)?;
    VersionReq::parse(&req).with_context(|| format!("failed to parse narrowed requirement {req}"))
}

pub(crate) fn version_req_is_broad(req: &VersionReq) -> anyhow::Result<bool> {
    let bounds = version_req_bounds(req)?;
    let Some(lower) = &bounds.lower else {
        return Ok(true);
    };
    let Some(upper) = &bounds.upper else {
        return Ok(true);
    };

    let min_version = minimum_satisfying_version(lower)?;
    let compatibility_bounds = compatibility_bounds(&min_version)?;
    let requirement_bounds =
        VersionReqBounds::range(VersionBound::inclusive(min_version), upper.clone());
    Ok(!version_bounds_subset(
        &requirement_bounds,
        &compatibility_bounds,
    ))
}

fn format_version_req_bounds(bounds: &VersionReqBounds) -> anyhow::Result<String> {
    let Some(lower) = &bounds.lower else {
        bail!("narrowed requirement must have a lower bound");
    };
    let Some(upper) = &bounds.upper else {
        bail!("narrowed requirement must have an upper bound");
    };
    if lower.version == upper.version && lower.inclusive && upper.inclusive {
        return Ok(format!("={}", lower.version));
    }
    let lower_op = if lower.inclusive { ">=" } else { ">" };
    let upper_op = if upper.inclusive { "<=" } else { "<" };
    Ok(format!(
        "{lower_op}{}, {upper_op}{}",
        lower.version, upper.version
    ))
}

fn comparator_bounds(comparator: &semver::Comparator) -> anyhow::Result<VersionReqBounds> {
    let base = comparator_version(comparator);
    let bounds = match comparator.op {
        semver::Op::Exact => exact_bounds(comparator, base)?,
        semver::Op::Greater => VersionReqBounds::lower(greater_lower_bound(comparator, base)?),
        semver::Op::GreaterEq => VersionReqBounds::lower(VersionBound::inclusive(base)),
        semver::Op::Less => VersionReqBounds::upper(VersionBound::exclusive(base)),
        semver::Op::LessEq => VersionReqBounds::upper(VersionBound::new(
            less_eq_upper_bound(comparator)?,
            comparator.patch.is_some(),
        )),
        semver::Op::Tilde => VersionReqBounds::range(
            VersionBound::inclusive(base),
            VersionBound::exclusive(tilde_upper_bound(comparator)?),
        ),
        semver::Op::Caret => VersionReqBounds::range(
            VersionBound::inclusive(base),
            VersionBound::exclusive(caret_upper_bound(comparator)?),
        ),
        semver::Op::Wildcard => wildcard_bounds(comparator)?,
        _ => bail!("unsupported semver comparator op {:?}", comparator.op),
    };
    Ok(bounds)
}

fn exact_bounds(
    comparator: &semver::Comparator,
    base: Version,
) -> anyhow::Result<VersionReqBounds> {
    if let Some(bounds) = partial_version_bounds(comparator)? {
        return Ok(bounds);
    }
    Ok(VersionReqBounds::exact(base))
}

fn comparator_version(comparator: &semver::Comparator) -> Version {
    let mut version = Version::new(
        comparator.major,
        comparator.minor.unwrap_or(0),
        comparator.patch.unwrap_or(0),
    );
    version.pre = comparator.pre.clone();
    version
}

fn partial_version_bounds(
    comparator: &semver::Comparator,
) -> anyhow::Result<Option<VersionReqBounds>> {
    let Some(minor) = comparator.minor else {
        return Ok(Some(VersionReqBounds::range(
            VersionBound::inclusive(Version::new(comparator.major, 0, 0)),
            VersionBound::exclusive(next_major(comparator.major)?),
        )));
    };
    if comparator.patch.is_some() {
        return Ok(None);
    }
    Ok(Some(VersionReqBounds::range(
        VersionBound::inclusive(Version::new(comparator.major, minor, 0)),
        VersionBound::exclusive(next_minor(comparator.major, minor)?),
    )))
}

fn greater_lower_bound(
    comparator: &semver::Comparator,
    base: Version,
) -> anyhow::Result<VersionBound> {
    if comparator.patch.is_some() {
        return Ok(VersionBound::exclusive(base));
    }
    if let Some(minor) = comparator.minor {
        return Ok(VersionBound::inclusive(next_minor(
            comparator.major,
            minor,
        )?));
    }
    Ok(VersionBound::inclusive(next_major(comparator.major)?))
}

fn less_eq_upper_bound(comparator: &semver::Comparator) -> anyhow::Result<Version> {
    if comparator.patch.is_some() {
        return Ok(comparator_version(comparator));
    }
    if let Some(minor) = comparator.minor {
        return next_minor(comparator.major, minor);
    }
    next_major(comparator.major)
}

fn wildcard_bounds(comparator: &semver::Comparator) -> anyhow::Result<VersionReqBounds> {
    if let Some(bounds) = partial_version_bounds(comparator)? {
        return Ok(bounds);
    }
    let version = comparator_version(comparator);
    Ok(VersionReqBounds::range(
        VersionBound::inclusive(version.clone()),
        VersionBound::exclusive(next_patch(&version)?),
    ))
}

fn caret_upper_bound(comparator: &semver::Comparator) -> anyhow::Result<Version> {
    if comparator.major > 0 {
        return next_major(comparator.major);
    }
    let Some(minor) = comparator.minor else {
        return next_major(comparator.major);
    };
    if minor > 0 {
        return next_minor(comparator.major, minor);
    }
    let Some(patch) = comparator.patch else {
        return next_minor(comparator.major, minor);
    };
    Ok(Version::new(
        comparator.major,
        minor,
        patch.checked_add(1).context("patch version overflow")?,
    ))
}

fn tilde_upper_bound(comparator: &semver::Comparator) -> anyhow::Result<Version> {
    let Some(minor) = comparator.minor else {
        return next_major(comparator.major);
    };
    next_minor(comparator.major, minor)
}

fn minimum_satisfying_version(lower: &VersionBound) -> anyhow::Result<Version> {
    if lower.inclusive || !lower.version.pre.is_empty() {
        return Ok(lower.version.clone());
    }
    next_patch(&lower.version)
}

fn compatibility_bounds(version: &Version) -> anyhow::Result<VersionReqBounds> {
    Ok(VersionReqBounds::range(
        VersionBound::inclusive(version.clone()),
        VersionBound::exclusive(semver_compatibility_upper_bound(version)?),
    ))
}

fn compatibility_lane_bounds(version: &Version) -> anyhow::Result<VersionReqBounds> {
    let lane = version_compatibility_lane(version)?;
    Ok(VersionReqBounds::range(
        VersionBound::inclusive(lane.lower),
        VersionBound::exclusive(lane.upper),
    ))
}

pub(crate) fn version_compatibility_lane(version: &Version) -> anyhow::Result<CompatibilityLane> {
    Ok(CompatibilityLane {
        lower: semver_compatibility_lower_bound(version),
        upper: semver_compatibility_upper_bound(version)?,
    })
}

fn semver_compatibility_lower_bound(version: &Version) -> Version {
    if !version.pre.is_empty() {
        return version.clone();
    }
    if version.major > 0 {
        return Version::new(version.major, 0, 0);
    }
    if version.minor > 0 {
        return Version::new(version.major, version.minor, 0);
    }
    version.clone()
}

fn semver_compatibility_upper_bound(version: &Version) -> anyhow::Result<Version> {
    if version.major > 0 {
        return next_major(version.major);
    }
    if version.minor > 0 {
        return next_minor(version.major, version.minor);
    }
    next_patch(version)
}

fn next_major(major: u64) -> anyhow::Result<Version> {
    Ok(Version::new(
        major.checked_add(1).context("major version overflow")?,
        0,
        0,
    ))
}

fn next_minor(major: u64, minor: u64) -> anyhow::Result<Version> {
    Ok(Version::new(
        major,
        minor.checked_add(1).context("minor version overflow")?,
        0,
    ))
}

fn next_patch(version: &Version) -> anyhow::Result<Version> {
    Ok(Version::new(
        version.major,
        version.minor,
        version
            .patch
            .checked_add(1)
            .context("patch version overflow")?,
    ))
}

fn merge_lower_bound(current: &mut Option<VersionBound>, candidate: Option<VersionBound>) {
    let Some(candidate) = candidate else {
        return;
    };
    if current
        .as_ref()
        .is_none_or(|current| lower_bound_after(&candidate, current))
    {
        *current = Some(candidate);
    }
}

fn merge_upper_bound(current: &mut Option<VersionBound>, candidate: Option<VersionBound>) {
    let Some(candidate) = candidate else {
        return;
    };
    if current
        .as_ref()
        .is_none_or(|current| upper_bound_before(&candidate, current))
    {
        *current = Some(candidate);
    }
}

fn lower_bound_after(candidate: &VersionBound, current: &VersionBound) -> bool {
    candidate.version > current.version
        || (candidate.version == current.version && !candidate.inclusive && current.inclusive)
}

fn upper_bound_before(candidate: &VersionBound, current: &VersionBound) -> bool {
    candidate.version < current.version
        || (candidate.version == current.version && !candidate.inclusive && current.inclusive)
}

fn lower_bound_subset(narrowed: &Option<VersionBound>, original: &Option<VersionBound>) -> bool {
    let Some(original) = original else {
        return true;
    };
    let Some(narrowed) = narrowed else {
        return false;
    };
    narrowed.version > original.version
        || (narrowed.version == original.version && (original.inclusive || !narrowed.inclusive))
}

fn upper_bound_subset(narrowed: &Option<VersionBound>, original: &Option<VersionBound>) -> bool {
    let Some(original) = original else {
        return true;
    };
    let Some(narrowed) = narrowed else {
        return false;
    };
    narrowed.version < original.version
        || (narrowed.version == original.version && (original.inclusive || !narrowed.inclusive))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_broad(req: &str) -> bool {
        version_req_is_broad(&VersionReq::parse(req).unwrap()).unwrap()
    }

    fn narrow(req: &str, version: &str) -> String {
        version_req_to_compatibility_lane(
            &VersionReq::parse(req).unwrap(),
            &Version::parse(version).unwrap(),
        )
        .unwrap()
        .to_string()
    }

    #[test]
    fn common_single_compatibility_ranges_are_not_broad() {
        assert!(!is_broad("^1"));
        assert!(!is_broad("^0.14.6"));
        assert!(!is_broad("~1.4"));
        assert!(!is_broad("~0.1.0"));
        assert!(!is_broad(">=1.0.100, <1.0.200"));
        assert!(!is_broad("=1"));
        assert!(!is_broad("=0.1"));
        assert!(!is_broad("=0.0.1"));
    }

    #[test]
    fn ranges_spanning_multiple_compatibility_ranges_are_broad() {
        assert!(is_broad("*"));
        assert!(is_broad(">=1"));
        assert!(is_broad(">=1, <3"));
        assert!(is_broad(">=0.1, <0.3"));
        assert!(is_broad("~0"));
        assert!(is_broad("^0"));
        assert!(is_broad("^0.0"));
        assert!(is_broad("=0"));
        assert!(is_broad("=0.0"));
    }

    #[test]
    fn compatibility_lane_narrowing_preserves_original_bounds() {
        assert_eq!(narrow(">=1.4, <2.1", "2.0.3"), ">=2.0.0, <2.1.0");
        assert_eq!(narrow(">=1.4, <3", "1.9.0"), ">=1.4.0, <2.0.0");
        assert_eq!(narrow("*", "0.2.7"), ">=0.2.0, <0.3.0");
        assert_eq!(narrow(">=0.0.1, <0.0.3", "0.0.2"), ">=0.0.2, <0.0.3");
    }

    #[test]
    fn greater_than_partial_versions_follow_semver_semantics() {
        assert_eq!(narrow(">1", "2.3.4"), ">=2.0.0, <3.0.0");
        assert_eq!(narrow(">1.0", "1.3.4"), ">=1.1.0, <2.0.0");
        assert_eq!(narrow(">1.0.0", "1.0.1"), ">1.0.0, <2.0.0");
    }
}
