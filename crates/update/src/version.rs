use std::fmt;

/// A `MAJOR.MINOR.PATCH` version, compared numerically (`0.10.0 > 0.9.9`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version {
    major: u64,
    minor: u64,
    patch: u64,
}

impl Version {
    /// Parses `1.2.3`, `v1.2.3` or `1.2.3-rc1` (a pre-release or build suffix is ignored).
    /// Missing minor or patch parts count as 0.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let core = text.trim().trim_start_matches('v');
        let core = core.split(['-', '+']).next()?;
        let mut parts = core.split('.');
        let mut next = |required: bool| match parts.next() {
            Some(part) => part.parse::<u64>().ok(),
            None if required => None,
            None => Some(0),
        };
        let version = Self {
            major: next(true)?,
            minor: next(false)?,
            patch: next(false)?,
        };
        parts.next().is_none().then_some(version)
    }

    /// The version this crate was built as; the workspace's single source of truth.
    #[must_use]
    pub fn current() -> Self {
        Self::parse(env!("CARGO_PKG_VERSION")).expect("the crate version is MAJOR.MINOR.PATCH")
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tags() {
        let v = |s| Version::parse(s).expect(s);
        assert_eq!(v("v1.2.3").to_string(), "1.2.3");
        assert_eq!(v("1.2.3-rc1"), v("1.2.3"));
        assert_eq!(v("2"), v("2.0.0"));
        assert!(Version::parse("1.x.3").is_none());
        assert!(Version::parse("1.2.3.4").is_none());
        assert!(Version::parse("").is_none());
    }

    #[test]
    fn compares_numerically() {
        let v = |s| Version::parse(s).expect(s);
        assert!(v("0.10.0") > v("0.9.9"));
        assert!(v("1.0.0") > v("0.99.99"));
        assert!(v("0.5.0") == v("v0.5.0"));
    }

    #[test]
    fn current_version_parses() {
        assert_eq!(Version::current().to_string(), env!("CARGO_PKG_VERSION"));
    }
}
