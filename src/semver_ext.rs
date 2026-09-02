/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

#![allow(clippy::manual_map)]

use std::cmp;
use std::cmp::Ordering;
use std::collections::BTreeSet;

use semver::Comparator;
use semver::Op;
use semver::Prerelease;
use semver::Version;
use semver::VersionReq;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct VersionBounds {
    lower: LowerBound,
    upper: UpperBound,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LowerBound {
    major: u64,
    minor: u64,
    patch: u64,
    pre: Prerelease,
    /// Invariant: if exclusive, Prerelease must be nonempty.
    exclusive: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct UpperBound {
    major: u64,
    minor: u64,
    patch: u64,
    pre: Prerelease,
    /// Invariant: if exclusive, Prerelease must be != "0".
    exclusive: bool,
}

// `None` if version req is unsatisfiable.
pub(crate) fn version_req_bounds(req: &VersionReq) -> Option<VersionBounds> {
    let mut prereleases = BTreeSet::new();
    for comparator in &req.comparators {
        if !comparator.pre.is_empty() {
            prereleases.insert((
                comparator.major,
                comparator.minor.unwrap(),
                comparator.patch.unwrap(),
            ));
        }
    }

    let mut lower_bounds = Vec::new();
    let mut upper_bounds = Vec::new();
    for comparator in &req.comparators {
        match comparator.op {
            Op::Exact => {
                let (lower, upper) = exact_bounds(comparator);
                lower_bounds.push(lower);
                upper_bounds.push(upper);
            }
            Op::Greater => {
                let lower = greater_lower_bound(comparator, &prereleases)?;
                lower_bounds.push(lower);
            }
            Op::GreaterEq => {
                let lower = greater_eq_lower_bound(comparator);
                lower_bounds.push(lower);
            }
            Op::Less => {
                let upper = less_upper_bound(comparator, &prereleases)?;
                upper_bounds.push(upper);
            }
            Op::LessEq => {
                let upper = less_eq_upper_bound(comparator);
                upper_bounds.push(upper);
            }
            Op::Tilde => {
                let (lower, upper) = tilde_bounds(comparator);
                lower_bounds.push(lower);
                upper_bounds.push(upper);
            }
            Op::Caret => {
                let (lower, upper) = caret_bounds(comparator, &prereleases);
                lower_bounds.push(lower);
                upper_bounds.push(upper);
            }
            Op::Wildcard => {
                let (lower, upper) = wildcard_bounds(comparator);
                lower_bounds.push(lower);
                upper_bounds.push(upper);
            }
            op => unimplemented!("unsupported semver comparator {op:?}"),
        }
    }

    let lower = lower_bounds.into_iter().max().unwrap_or_else(|| {
        LowerBound {
            major: 0,
            minor: 0,
            patch: 0,
            pre: if prereleases.contains(&(0, 0, 0)) {
                // ">=0.0.0-0"
                Prerelease::new("0").unwrap()
            } else {
                // ">=0.0.0"
                Prerelease::EMPTY
            },
            exclusive: false,
        }
    });

    let upper = upper_bounds.into_iter().min().unwrap_or_else(|| {
        // "<=18446744073709551615.18446744073709551615.18446744073709551615"
        UpperBound {
            major: u64::MAX,
            minor: u64::MAX,
            patch: u64::MAX,
            pre: Prerelease::EMPTY,
            exclusive: false,
        }
    });

    // Check satisfiability by comparing major then minor then patch
    // then prerelease. If all four equal, then satisfiability reduces to
    // whether both bounds are inclusive.
    if (
        lower.major,
        lower.minor,
        lower.patch,
        &lower.pre,
        lower.exclusive,
    ) < (
        upper.major,
        upper.minor,
        upper.patch,
        &upper.pre,
        !upper.exclusive,
    ) {
        Some(VersionBounds { lower, upper })
    } else {
        None
    }
}

pub(crate) fn version_bounds_subset(narrowed: &VersionBounds, original: &VersionBounds) -> bool {
    original.lower <= narrowed.lower && narrowed.upper <= original.upper
}

pub(crate) fn version_req_to_compatibility_lane(req: &VersionReq, version: &Version) -> VersionReq {
    assert!(req.matches(version));

    let original_bounds = version_req_bounds(req).expect("req is definitely satisfiable");

    let compatibility_lane = Comparator {
        op: Op::Caret,
        major: version.major,
        minor: Some(if version.major == 0 { version.minor } else { 0 }),
        patch: Some(if version.major == 0 && version.minor == 0 {
            version.patch
        } else {
            0
        }),
        pre: version.pre.clone(),
    };
    let (lane_lower, lane_upper) = caret_bounds(&compatibility_lane, &BTreeSet::new());

    let lower = cmp::max(original_bounds.lower, lane_lower);
    let upper = cmp::min(original_bounds.upper, lane_upper);
    let mut compatibility_lane_req = VersionReq {
        comparators: Vec::with_capacity(2),
    };
    compatibility_lane_req.comparators.push(Comparator {
        op: if lower.exclusive {
            Op::Greater
        } else {
            Op::GreaterEq
        },
        major: lower.major,
        minor: Some(lower.minor),
        patch: Some(lower.patch),
        pre: lower.pre,
    });
    if !upper.exclusive
        && upper.pre.is_empty()
        && let (next_patch, carry) = upper.patch.overflowing_add(1)
        && let (next_minor, carry) = upper.minor.overflowing_add(u64::from(carry))
        && let Some(next_major) = upper.major.checked_add(u64::from(carry))
    {
        compatibility_lane_req.comparators.push(Comparator {
            op: Op::Less,
            major: next_major,
            minor: Some(next_minor),
            patch: Some(next_patch),
            pre: Prerelease::EMPTY,
        });
    } else {
        compatibility_lane_req.comparators.push(Comparator {
            op: if upper.exclusive {
                Op::Less
            } else {
                Op::LessEq
            },
            major: upper.major,
            minor: Some(upper.minor),
            patch: Some(upper.patch),
            pre: upper.pre,
        });
    }
    compatibility_lane_req
}

pub(crate) fn version_req_is_broad(req: &VersionReq) -> bool {
    let Some(bounds) = version_req_bounds(req) else {
        return false;
    };

    // A version req is broad if the greatest satisfying version is not
    // semver-compatible with the smallest satisfying version.
    let compatible_with_lower = Comparator {
        op: Op::Caret,
        major: bounds.lower.major,
        minor: Some(bounds.lower.minor),
        patch: Some(bounds.lower.patch),
        pre: Prerelease::EMPTY,
    };
    let upper = Version::new(bounds.upper.major, bounds.upper.minor, bounds.upper.patch);
    !compatible_with_lower.matches(&upper)
}

fn exact_bounds(comparator: &Comparator) -> (LowerBound, UpperBound) {
    // "=I.J.K-alpha"  =>  ">=I.J.K-alpha, <=I.J.K-alpha"
    // "=I.J.K"  =>  ">=I.J.K, <=I.J.K"
    // "=I.J"  =>  ">=I.J.0, <=I.J.MAX"
    // "=I"  =>  ">=I.0.0, <=I.MAX.MAX"
    let lower = LowerBound {
        major: comparator.major,
        minor: comparator.minor.unwrap_or(0),
        patch: comparator.patch.unwrap_or(0),
        pre: comparator.pre.clone(),
        exclusive: false,
    };
    let upper = UpperBound {
        major: comparator.major,
        minor: comparator.minor.unwrap_or(u64::MAX),
        patch: comparator.patch.unwrap_or(u64::MAX),
        pre: comparator.pre.clone(),
        exclusive: false,
    };
    (lower, upper)
}

// `None` if comparator is unsatisfiable.
fn greater_lower_bound(
    comparator: &Comparator,
    prereleases: &BTreeSet<(u64, u64, u64)>,
) -> Option<LowerBound> {
    if !comparator.pre.is_empty() {
        // ">I.J.K-alpha"
        Some(LowerBound {
            major: comparator.major,
            minor: comparator.minor.unwrap(),
            patch: comparator.patch.unwrap(),
            pre: comparator.pre.clone(),
            // Invariant: prerelease != ""
            exclusive: true,
        })
    } else if let Some(patch) = comparator.patch
        && let Some(next_patch) = patch.checked_add(1)
    {
        // ">I.J.K"  =>  ">=I.J.(K+1)" or ">=I.J.(K+1)-0"
        Some(LowerBound {
            major: comparator.major,
            minor: comparator.minor.unwrap(),
            patch: next_patch,
            pre: if prereleases.contains(&(comparator.major, comparator.minor.unwrap(), next_patch))
            {
                Prerelease::new("0").unwrap()
            } else {
                Prerelease::EMPTY
            },
            exclusive: false,
        })
    } else if let Some(minor) = comparator.minor
        && let Some(next_minor) = minor.checked_add(1)
    {
        // ">I.J"  =>  ">=I.(J+1).0" or ">=I.(J+1).0-0"
        // ">I.J.MAX"  =>  same
        Some(LowerBound {
            major: comparator.major,
            minor: next_minor,
            patch: 0,
            pre: if prereleases.contains(&(comparator.major, next_minor, 0)) {
                Prerelease::new("0").unwrap()
            } else {
                Prerelease::EMPTY
            },
            exclusive: false,
        })
    } else if let Some(next_major) = comparator.major.checked_add(1) {
        // ">I"  =>  ">=(I+1).0.0" or ">=(I+1).0.0-0"
        // ">I.MAX"  =>  same
        // ">I.MAX.MAX"  =>  same
        Some(LowerBound {
            major: next_major,
            minor: 0,
            patch: 0,
            pre: if prereleases.contains(&(next_major, 0, 0)) {
                Prerelease::new("0").unwrap()
            } else {
                Prerelease::EMPTY
            },
            exclusive: false,
        })
    } else {
        // ">MAX.MAX.MAX"  =>  unsatisfiable
        None
    }
}

fn greater_eq_lower_bound(comparator: &Comparator) -> LowerBound {
    // ">=I.J.K-alpha"
    // ">=I.J.K"
    // ">=I.J"  =>  ">=I.J.0"
    // ">=I"  =>  ">=I.0.0"
    LowerBound {
        major: comparator.major,
        minor: comparator.minor.unwrap_or(0),
        patch: comparator.patch.unwrap_or(0),
        pre: comparator.pre.clone(),
        exclusive: false,
    }
}

fn less_upper_bound(
    comparator: &Comparator,
    prereleases: &BTreeSet<(u64, u64, u64)>,
) -> Option<UpperBound> {
    if !matches!(comparator.pre.as_str(), "" | "0") {
        // "<I.J.K-alpha"
        Some(UpperBound {
            major: comparator.major,
            minor: comparator.minor.unwrap(),
            patch: comparator.patch.unwrap(),
            pre: comparator.pre.clone(),
            // Invariant: prerelease != "0"
            exclusive: true,
        })
    } else if let Some(minor) = comparator.minor
        && let Some(patch) = comparator.patch
        && prereleases.contains(&(comparator.major, minor, patch))
    {
        // "<I.J.K" allowing pre-releases
        Some(UpperBound {
            major: comparator.major,
            minor,
            patch,
            pre: Prerelease::EMPTY,
            // Invariant: prerelease != "0"
            exclusive: true,
        })
    } else if let Some(patch) = comparator.patch
        && let Some(prev_patch) = patch.checked_sub(1)
    {
        // "<I.J.K"  =>  "<=I.J.(K-1)"
        Some(UpperBound {
            major: comparator.major,
            minor: comparator.minor.unwrap(),
            patch: prev_patch,
            pre: Prerelease::EMPTY,
            exclusive: false,
        })
    } else if let Some(minor) = comparator.minor
        && let Some(prev_minor) = minor.checked_sub(1)
    {
        // "<I.J"  =>  "<=I.(J-1).MAX"
        // "<I.J.0"  =>  same
        Some(UpperBound {
            major: comparator.major,
            minor: prev_minor,
            patch: u64::MAX,
            pre: Prerelease::EMPTY,
            exclusive: false,
        })
    } else if let Some(prev_major) = comparator.major.checked_sub(1) {
        // "<I"  =>  "<=(I-1).MAX.MAX"
        // "<I.0"  =>  same
        // "<I.0.0"  =>  same
        Some(UpperBound {
            major: prev_major,
            minor: u64::MAX,
            patch: u64::MAX,
            pre: Prerelease::EMPTY,
            exclusive: false,
        })
    } else {
        // "<0.0.0", "<0.0", "<0"  =>  unsatisfiable
        None
    }
}

fn less_eq_upper_bound(comparator: &Comparator) -> UpperBound {
    // "<=I.J.K-alpha"
    // "<=I.J.K"
    // "<=I.J"  =>  "<=I.J.MAX"
    // "<=I"  =>  "<=I.MAX.MAX"
    UpperBound {
        major: comparator.major,
        minor: comparator.minor.unwrap_or(u64::MAX),
        patch: comparator.patch.unwrap_or(u64::MAX),
        pre: comparator.pre.clone(),
        exclusive: false,
    }
}

fn tilde_bounds(comparator: &Comparator) -> (LowerBound, UpperBound) {
    // "~I.J.K-alpha"  =>  ">=I.J.K-alpha, <=I.J.MAX"
    // "~I.J.K"  =>  ">=I.J.K, <=I.J.MAX"
    // "~I.J"  =>  ">=I.J.0, <=I.J.MAX"
    // "~I"  =>  ">=I.0.0, <=I.MAX.MAX"
    let lower = LowerBound {
        major: comparator.major,
        minor: comparator.minor.unwrap_or(0),
        patch: comparator.patch.unwrap_or(0),
        pre: comparator.pre.clone(),
        exclusive: false,
    };
    let upper = UpperBound {
        major: comparator.major,
        minor: comparator.minor.unwrap_or(u64::MAX),
        patch: u64::MAX,
        pre: Prerelease::EMPTY,
        exclusive: false,
    };
    (lower, upper)
}

fn caret_bounds(
    comparator: &Comparator,
    prereleases: &BTreeSet<(u64, u64, u64)>,
) -> (LowerBound, UpperBound) {
    let lower = LowerBound {
        major: comparator.major,
        minor: comparator.minor.unwrap_or(0),
        patch: comparator.patch.unwrap_or(0),
        pre: if !comparator.pre.is_empty() {
            comparator.pre.clone()
        } else if comparator.patch.is_none()
            && prereleases.contains(&(comparator.major, comparator.minor.unwrap_or(0), 0))
        {
            Prerelease::new("0").unwrap()
        } else {
            Prerelease::EMPTY
        },
        exclusive: false,
    };

    let upper = if comparator.major != 0 || comparator.minor.is_none() {
        // "^I.J.K" (for I>0)  =>  ">=I.J.K, <=I.MAX.MAX"
        // "^I"  =>  ">=I.0.0, <=I.MAX.MAX"
        UpperBound {
            major: comparator.major,
            minor: u64::MAX,
            patch: u64::MAX,
            pre: Prerelease::EMPTY,
            exclusive: false,
        }
    } else if let Some(minor @ 1..) = comparator.minor {
        // "^0.J.K" (for J>0)  =>  ">=0.J.K, <=0.J.MAX"
        UpperBound {
            major: 0,
            minor,
            patch: u64::MAX,
            pre: Prerelease::EMPTY,
            exclusive: false,
        }
    } else {
        // "^0.0"  =>  ">=0.0.0, <=0.0.MAX"
        // "^0.0.K"  =>  ">=0.0.K, <=0.0.K"
        UpperBound {
            major: 0,
            minor: 0,
            patch: comparator.patch.unwrap_or(u64::MAX),
            pre: Prerelease::EMPTY,
            exclusive: false,
        }
    };

    (lower, upper)
}

fn wildcard_bounds(comparator: &Comparator) -> (LowerBound, UpperBound) {
    // "I.J.*"  =>  ">=I.J.0, <=I.J.MAX"
    // "I.*"  =>  ">=I.0.0, <=I.MAX.MAX"
    let lower = LowerBound {
        major: comparator.major,
        minor: comparator.minor.unwrap_or(0),
        patch: 0,
        pre: Prerelease::EMPTY,
        exclusive: false,
    };
    let upper = UpperBound {
        major: comparator.major,
        minor: comparator.minor.unwrap_or(u64::MAX),
        patch: u64::MAX,
        pre: Prerelease::EMPTY,
        exclusive: false,
    };
    (lower, upper)
}

/// Comparison order: ">=1.0.0-alpha" < ">1.0.0-alpha" < ">=1.0.0"
impl PartialOrd for LowerBound {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(Self::cmp(self, other))
    }
}
impl Ord for LowerBound {
    fn cmp(&self, other: &Self) -> Ordering {
        Ord::cmp(
            &(
                self.major,
                self.minor,
                self.patch,
                &self.pre,
                self.exclusive,
            ),
            &(
                other.major,
                other.minor,
                other.patch,
                &other.pre,
                other.exclusive,
            ),
        )
    }
}

/// Comparison order: "<1.0.0-alpha" < "<=1.0.0-alpha" < "<1.0.0" < "<=1.0.0"
impl PartialOrd for UpperBound {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(Self::cmp(self, other))
    }
}
impl Ord for UpperBound {
    fn cmp(&self, other: &Self) -> Ordering {
        Ord::cmp(
            &(
                self.major,
                self.minor,
                self.patch,
                &self.pre,
                !self.exclusive,
            ),
            &(
                other.major,
                other.minor,
                other.patch,
                &other.pre,
                !other.exclusive,
            ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_broad(req: &str) -> bool {
        version_req_is_broad(&VersionReq::parse(req).unwrap())
    }

    fn narrow(req: &str, version: &str) -> String {
        version_req_to_compatibility_lane(
            &VersionReq::parse(req).unwrap(),
            &Version::parse(version).unwrap(),
        )
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
        assert_eq!(narrow(">1.0.0", "1.0.1"), ">=1.0.1, <2.0.0");
    }
}
