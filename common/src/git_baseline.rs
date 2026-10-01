//! Git baseline label carried inside encrypted sync snapshots.
//!
//! Every machine keeps its own Git clone; FeanorFS shares only the
//! uncommitted work on top. Each sync snapshot records the commit (and
//! branch) its publisher's clone sat on, so other machines can warn when
//! they apply that work on a different baseline. The label lives in the
//! encrypted `Snapshot.message`; the hub never sees it.

use serde::{Deserialize, Serialize};

/// Discriminator of the baseline label in `Snapshot.message`.
pub const GIT_BASELINE_DISCRIMINATOR: &str = "ffbase1";
/// Longest branch name carried in a label; longer names are omitted.
pub const GIT_BRANCH_MAX_BYTES: usize = 255;

/// The commit a Git clone's `HEAD` resolves to, and its branch unless
/// detached.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitBaseline {
    /// Full SHA-1 (40) or SHA-256 (64) lowercase hex object id.
    pub commit: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

/// This machine's baseline differs from the one the shared work was
/// published on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitBaselineMismatch {
    pub local: GitBaseline,
    pub shared: GitBaseline,
}

impl GitBaseline {
    /// Builds a validated baseline; invalid commits yield `None` and
    /// invalid branch names are dropped.
    #[must_use]
    pub fn new(commit: &str, branch: Option<&str>) -> Option<Self> {
        is_object_id(commit).then(|| Self {
            commit: commit.to_string(),
            branch: branch.filter(|b| is_branch_name(b)).map(str::to_string),
        })
    }

    /// `ffbase1:<commit>:<branch>` (empty branch when detached). Git ref
    /// names never contain `:`, so the encoding is unambiguous.
    #[must_use]
    pub fn encode(&self) -> String {
        format!(
            "{GIT_BASELINE_DISCRIMINATOR}:{}:{}",
            self.commit,
            self.branch.as_deref().unwrap_or_default()
        )
    }

    /// Parses a label; anything else (including signals) is `None`.
    #[must_use]
    pub fn parse(message: &str) -> Option<Self> {
        let rest = message
            .strip_prefix(GIT_BASELINE_DISCRIMINATOR)?
            .strip_prefix(':')?;
        let (commit, branch) = rest.split_once(':')?;
        let baseline = Self::new(commit, (!branch.is_empty()).then_some(branch))?;
        (baseline.branch.is_some() != branch.is_empty()).then_some(baseline)
    }

    /// Short human form: `main@1a2b3c4` or `1a2b3c4` when detached.
    #[must_use]
    pub fn short(&self) -> String {
        let commit = &self.commit[..7];
        match &self.branch {
            Some(branch) => format!("{branch}@{commit}"),
            None => commit.to_string(),
        }
    }
}

fn is_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn is_branch_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= GIT_BRANCH_MAX_BYTES
        && !value.contains(':')
        && !value.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_roundtrip_and_reject_noise() {
        let commit = "a".repeat(40);
        let branch = GitBaseline::new(&commit, Some("feature/parser")).unwrap();
        assert_eq!(branch.encode(), format!("ffbase1:{commit}:feature/parser"));
        assert_eq!(GitBaseline::parse(&branch.encode()), Some(branch.clone()));
        assert_eq!(branch.short(), "feature/parser@aaaaaaa");

        let detached = GitBaseline::new(&"b".repeat(64), None).unwrap();
        assert_eq!(GitBaseline::parse(&detached.encode()), Some(detached));

        assert!(GitBaseline::new("ABC", None).is_none());
        assert!(GitBaseline::parse("ffmsg1:{}").is_none());
        assert!(GitBaseline::parse(&format!("ffbase1:{commit}:bad\nname")).is_none());
        assert!(GitBaseline::parse(&format!("ffbase1:{}:main", "A".repeat(40))).is_none());
    }
}
