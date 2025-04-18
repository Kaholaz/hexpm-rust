//! Functions for parsing and matching versions against requirements, based off
//! and compatible with the Elixir Version module, which is used by Hex
//! internally as well as be the Elixir build tool Hex client.

use std::{
    cell::RefCell,
    cmp::{Ordering, Reverse},
    collections::HashMap,
    convert::TryFrom,
    error::Error as StdError,
    fmt::{self, Display},
};

use crate::{Dependency, Package, Release};

use self::parser::Parser;
use pubgrub::{Dependencies, Map, PubGrubError};
use serde::{
    Deserialize, Serialize,
    de::{self, Deserializer},
};

mod lexer;
mod parser;
#[cfg(test)]
mod tests;

type PubgrubRange = pubgrub::Range<Version>;

pub fn exact(v: Version) -> PubgrubRange {
    let v1 = v.bump();
    PubgrubRange::between(v, v1)
}

/// In a nutshell, a version is represented by three numbers:
///
/// MAJOR.MINOR.PATCH
///
/// Pre-releases are supported by optionally appending a hyphen and a series of
/// period-separated identifiers immediately following the patch version.
/// Identifiers consist of only ASCII alphanumeric characters and hyphens (`[0-9A-Za-z-]`):
///
/// "1.0.0-alpha.3"
///
/// Build information can be added by appending a plus sign and a series of
/// dot-separated identifiers immediately following the patch or pre-release version.
/// Identifiers consist of only ASCII alphanumeric characters and hyphens (`[0-9A-Za-z-]`):
///
/// "1.0.0-alpha.3+20130417140000.amd64"
///
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
    pub pre: Vec<Identifier>,
    pub build: Option<String>,
}

impl Version {
    pub fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
            pre: vec![],
            build: None,
        }
    }

    fn bump_major(&self) -> Self {
        Self {
            major: self.major + 1,
            minor: 0,
            patch: 0,
            pre: vec![],
            build: None,
        }
    }

    fn bump_minor(&self) -> Self {
        Self {
            major: self.major,
            minor: self.minor + 1,
            patch: 0,
            pre: vec![],
            build: None,
        }
    }

    fn bump_patch(&self) -> Self {
        Self {
            major: self.major,
            minor: self.minor,
            patch: self.patch + 1,
            pre: vec![],
            build: None,
        }
    }

    /// Parse a version.
    pub fn parse(input: &str) -> Result<Self, parser::Error> {
        let mut parser = Parser::new(input)?;
        let version = parser.version()?;
        if !parser.is_eof() {
            return Err(parser::Error::MoreInput(
                parser
                    .tail()?
                    .into_iter()
                    .map(|t| t.to_string())
                    .collect::<Vec<_>>()
                    .join(""),
            ));
        }
        Ok(version)
    }

    /// Parse a Hex compatible version range. i.e. `> 1 and < 2 or == 4.5.2`.
    fn parse_range(input: &str) -> Result<PubgrubRange, parser::Error> {
        let mut parser = Parser::new(input)?;
        let version = parser.range()?;
        if !parser.is_eof() {
            return Err(parser::Error::MoreInput(
                parser
                    .tail()?
                    .into_iter()
                    .map(|t| t.to_string())
                    .collect::<Vec<_>>()
                    .join(""),
            ));
        }
        Ok(version)
    }

    fn tuple(&self) -> (u32, u32, u32, PreOrder<'_>) {
        (
            self.major,
            self.minor,
            self.patch,
            PreOrder(self.pre.as_slice()),
        )
    }

    pub fn is_pre(&self) -> bool {
        !self.pre.is_empty()
    }
}

impl<'de> Deserialize<'de> for Version {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s: &str = Deserialize::deserialize(deserializer)?;
        Version::try_from(s).map_err(de::Error::custom)
    }
}

impl Serialize for Version {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl std::cmp::PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl std::cmp::Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.tuple().cmp(&other.tuple())
    }
}

impl Version {
    fn bump(&self) -> Self {
        if self.is_pre() {
            let mut pre = self.pre.clone();
            let last_component = pre
                .last_mut()
                // This `.expect` is safe, as we know there to be at least
                // one pre-release component.
                .expect("no pre-release components");

            match last_component {
                Identifier::Numeric(pre) => *pre += 1,
                Identifier::AlphaNumeric(pre) => {
                    let mut segments = split_alphanumeric(pre);
                    let last_segment = segments.last_mut().unwrap();

                    match last_segment {
                        AlphaOrNumeric::Numeric(n) => *n += 1,
                        AlphaOrNumeric::Alpha(alpha) => {
                            // We should potentially be smarter about this (for instance, pick the next letter in the
                            // alphabetic sequence), however, this seems like it could be quite a bit more complex.
                            alpha.push('1')
                        }
                    }

                    *pre = segments
                        .into_iter()
                        .map(|segment| match segment {
                            AlphaOrNumeric::Alpha(segment) => segment,
                            AlphaOrNumeric::Numeric(segment) => segment.to_string(),
                        })
                        .collect::<Vec<_>>()
                        .join("");
                }
            }

            Self {
                major: self.major,
                minor: self.minor,
                patch: self.patch,
                pre,
                build: None,
            }
        } else {
            self.bump_patch()
        }
    }
}

enum AlphaOrNumeric {
    Alpha(String),
    Numeric(u32),
}

/// Splits the given string into alphabetic and numeric segments.
fn split_alphanumeric(str: &str) -> Vec<AlphaOrNumeric> {
    let mut segments = Vec::new();
    let mut current_segment = String::new();
    let mut previous_char_was_numeric = None;

    for char in str.chars() {
        let is_numeric = char.is_ascii_digit();
        match previous_char_was_numeric {
            Some(previous_char_was_numeric) if previous_char_was_numeric == is_numeric => {
                current_segment.push(char)
            }
            _ => {
                if !current_segment.is_empty() {
                    if current_segment.chars().any(|char| char.is_ascii_digit()) {
                        segments.push(AlphaOrNumeric::Numeric(current_segment.parse().unwrap()));
                    } else {
                        segments.push(AlphaOrNumeric::Alpha(current_segment));
                    }

                    current_segment = String::new();
                }

                current_segment.push(char);
                previous_char_was_numeric = Some(is_numeric);
            }
        }
    }

    if !current_segment.is_empty() {
        if current_segment.chars().any(|char| char.is_ascii_digit()) {
            segments.push(AlphaOrNumeric::Numeric(current_segment.parse().unwrap()));
        } else {
            segments.push(AlphaOrNumeric::Alpha(current_segment));
        }
    }

    segments
}

impl<'a> TryFrom<&'a str> for Version {
    type Error = parser::Error;

    fn try_from(value: &'a str) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if !self.pre.is_empty() {
            write!(f, "-")?;
            for (i, identifier) in self.pre.iter().enumerate() {
                if i != 0 {
                    write!(f, ".")?;
                }
                identifier.fmt(f)?;
            }
        }
        if let Some(build) = self.build.as_ref() {
            write!(f, "+{}", build)?;
        }
        Ok(())
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Identifier {
    Numeric(u32),
    AlphaNumeric(String),
}

impl fmt::Display for Identifier {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Identifier::Numeric(ref id) => id.fmt(f),
            Identifier::AlphaNumeric(ref id) => id.fmt(f),
        }
    }
}

impl Identifier {
    pub fn concat(self, add_str: &str) -> Identifier {
        match self {
            Identifier::Numeric(n) => Identifier::AlphaNumeric(format!("{}{}", n, add_str)),
            Identifier::AlphaNumeric(mut s) => {
                s.push_str(add_str);
                Identifier::AlphaNumeric(s)
            }
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Range {
    spec: String,
    range: PubgrubRange,
}

impl Range {
    pub fn new(spec: String) -> Result<Self, parser::Error> {
        Version::parse_range(&spec).map(|range| Range { spec, range })
    }
}

impl<'de> Deserialize<'de> for Range {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s: &str = Deserialize::deserialize(deserializer)?;
        Range::new(s.to_string()).map_err(|e| serde::de::Error::custom(e.to_string()))
    }
}

impl Serialize for Range {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl fmt::Display for Range {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.spec)
    }
}

// A wrapper around Vec where an empty vector is greater than a non-empty one.
// This is desires as if there is a pre-segment in a version (1.0.0-rc1) it is
// lower than the same version with no pre-segments (1.0.0).
#[derive(PartialEq, Eq)]
pub struct PreOrder<'a>(&'a [Identifier]);

impl PreOrder<'_> {
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::cmp::PartialOrd for PreOrder<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl std::cmp::Ord for PreOrder<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        if self.is_empty() && other.is_empty() {
            Ordering::Equal
        } else if self.is_empty() {
            Ordering::Greater
        } else if other.is_empty() {
            Ordering::Less
        } else {
            self.0.cmp(other.0)
        }
    }
}

pub type PackageVersions = HashMap<String, Version>;

pub type ResolutionError<'a> = PubGrubError<DependencyProvider<'a>>;

pub fn resolve_versions<Requirements>(
    remote: Box<dyn PackageFetcher>,
    root_name: PackageName,
    dependencies: Requirements,
    locked: &HashMap<String, Version>,
) -> Result<PackageVersions, DependencyError<'_>>
where
    Requirements: Iterator<Item = (String, Range)>,
{
    let root_version = Version::new(0, 0, 0);
    let root = Package {
        name: root_name.clone(),
        repository: "local".to_string(),
        releases: vec![Release {
            version: root_version.clone(),
            outer_checksum: vec![],
            retirement_status: None,
            requirements: root_dependencies(dependencies, locked)?,
            meta: (),
        }],
    };
    let packages = pubgrub::resolve(
        &DependencyProvider::new(remote, root, locked),
        root_name.clone(),
        root_version,
    )?
    .into_iter()
    .filter(|(name, _)| name.as_str() != root_name.as_str())
    .collect();

    Ok(packages)
}

#[derive(Debug)]
pub enum DependencyError<'a> {
    IncompatibleLockedVersion(Box<IncompatibleLockedVersion>),
    ResolutionError(ResolutionError<'a>),
}
impl Display for DependencyError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msg = match self {
            DependencyError::IncompatibleLockedVersion(e) => e.to_string(),
            DependencyError::ResolutionError(e) => e.to_string(),
        };
        f.write_str(&msg)
    }
}
impl StdError for DependencyError<'_> {}
impl From<Box<IncompatibleLockedVersion>> for DependencyError<'_> {
    fn from(value: Box<IncompatibleLockedVersion>) -> Self {
        Self::IncompatibleLockedVersion(value)
    }
}
impl<'a> From<ResolutionError<'a>> for DependencyError<'a> {
    fn from(value: ResolutionError<'a>) -> Self {
        Self::ResolutionError(value)
    }
}

#[derive(Debug, Clone)]
pub struct IncompatibleLockedVersion {
    package: String,
    requirement: Range,
    version: Version,
}
impl Display for IncompatibleLockedVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{package} is specified with the requirement `{requirement}`, \
but it is locked to {version}, which is incompatible.",
            package = self.package,
            requirement = self.requirement.spec,
            version = self.version,
        )
    }
}

fn root_dependencies<Requirements>(
    base_requirements: Requirements,
    locked: &HashMap<String, Version>,
) -> Result<HashMap<String, Dependency>, Box<IncompatibleLockedVersion>>
where
    Requirements: Iterator<Item = (String, Range)>,
{
    // Record all of the already locked versions as hard requirements
    let mut requirements: HashMap<_, _> = locked
        .iter()
        .map(|(name, version)| (name.to_string(), Dependency::from_version(version)))
        .collect();

    for (name, range) in base_requirements {
        match locked.get(&name) {
            // If the package was not already locked then we can use the
            // specified version requirement without modification.
            None => {
                let _ = requirements.insert(name, Dependency::from_range(range));
            }

            // If the version was locked we verify that the requirement is
            // compatible with the locked version.
            Some(locked_version) => {
                let compatible = range.range.contains(locked_version);
                if !compatible {
                    return Err(Box::new(IncompatibleLockedVersion {
                        package: name,
                        requirement: range,
                        version: locked_version.clone(),
                    }));
                }
            }
        };
    }

    Ok(requirements)
    // let locked = locked
    //     .iter()
    //     .map(|(name, version)| (name.to_string(), Range::new(version.to_string())));
    // // Add the locked versions as new requirements that override any existing
    // // entry in the dependencies list. Collection into a HashMap is used for
    // // de-duplication.
    // let deps: HashMap<_, _> = dependencies.chain(locked).collect();
    // deps.into_iter()
    //     .map(|(package, requirement)| {
    //         (
    //             package,
    //             Dependency {
    //                 app: None,
    //                 optional: false,
    //                 repository: None,
    //                 requirement,
    //             },
    //         )
    //     })
    //     .collect()
}

pub trait PackageFetcher {
    fn get_dependencies(&self, package: &str) -> Result<Package, Box<dyn StdError>>;
}

#[derive(Debug, Clone)]
pub struct FetchError(String);
impl StdError for FetchError {}
impl Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

pub struct DependencyProvider<'a> {
    packages: RefCell<HashMap<String, Package>>,
    remote: Box<dyn PackageFetcher>,
    locked: &'a HashMap<String, Version>,
}

impl<'a> DependencyProvider<'a> {
    fn new(
        remote: Box<dyn PackageFetcher>,
        root: Package,
        locked: &'a HashMap<String, Version>,
    ) -> Self {
        let mut packages = HashMap::new();
        let _ = packages.insert(root.name.clone(), root);
        Self {
            packages: RefCell::new(packages),
            locked,
            remote,
        }
    }

    /// Download information about the package from the registry into the local
    /// store. Does nothing if the packages are already known.
    ///
    /// Package versions are sorted from newest to oldest, with all pre-releases
    /// at the end to ensure that a non-prerelease version will be picked first
    /// if there is one.
    //
    fn ensure_package_fetched(
        // We would like to use `&mut self` but the pubgrub library enforces
        // `&self` with interop mutability.
        &self,
        name: &str,
    ) -> Result<(), FetchError> {
        let mut packages = self.packages.borrow_mut();
        if packages.get(name).is_none() {
            let mut package = self
                .remote
                .get_dependencies(name)
                .map_err(|e| FetchError(e.to_string()))?;
            // Sort the packages from newest to oldest, pres after all others
            package.releases.sort_by(|a, b| a.version.cmp(&b.version));
            package.releases.reverse();
            let (pre, mut norm): (_, Vec<_>) =
                package.releases.into_iter().partition(Release::is_pre);
            norm.extend(pre);
            package.releases = norm;
            packages.insert(name.to_string(), package);
        }
        Ok(())
    }
}

type PackageName = String;

impl pubgrub::DependencyProvider for DependencyProvider<'_> {
    fn get_dependencies(
        &self,
        name: &Self::P,
        version: &Self::V,
    ) -> Result<pubgrub::Dependencies<Self::P, Self::VS, Self::M>, Self::Err> {
        self.ensure_package_fetched(name)?;
        let packages = self.packages.borrow();
        let release = match packages
            .get(name)
            .into_iter()
            .flat_map(|p| p.releases.iter())
            .find(|r| &r.version == version)
        {
            Some(release) => release,
            None => {
                return Ok(Dependencies::Unavailable(format!(
                    "version {version} of package {name} not found"
                )));
            }
        };

        // Only use retired versions if they have been locked
        if release.is_retired() && self.locked.get(name) != Some(version) {
            return Ok(Dependencies::Unavailable(format!(
                "version {version} of package {name} is retired"
            )));
        }

        let mut deps: Map<String, PubgrubRange> = Default::default();
        for (name, d) in &release.requirements {
            let range = &d.requirement.range;
            deps.insert(name.clone(), range.clone());
        }
        Ok(Dependencies::Available(deps))
    }

    fn prioritize(
        &self,
        name: &Self::P,
        range: &Self::VS,
        _package_conflicts_counts: &pubgrub::PackageResolutionStatistics,
    ) -> Self::Priority {
        Reverse(
            self.packages
                .borrow()
                .get(name)
                .into_iter()
                .flat_map(|p| &p.releases)
                .filter(|r| range.contains(&r.version))
                .count(),
        )
    }

    fn choose_version(
        &self,
        name: &Self::P,
        range: &Self::VS,
    ) -> Result<Option<Self::V>, Self::Err> {
        self.ensure_package_fetched(name)?;
        let packages = self.packages.borrow();
        let compatible_packages = packages
            .get(name)
            .into_iter()
            .flat_map(|p| &p.releases)
            .filter(|&r| range.contains(&r.version))
            .map(|r| r.version.clone());
        match compatible_packages.clone().filter(|v| !v.is_pre()).max() {
            // Don't resolve to a pre-releaase package unless we *have* to
            Some(v) => Ok(Some(v)),
            None => Ok(compatible_packages.max()),
        }
    }

    type Priority = Reverse<usize>;
    type Err = FetchError;
    type P = PackageName;
    type V = Version;
    type VS = PubgrubRange;
    type M = String;
}
