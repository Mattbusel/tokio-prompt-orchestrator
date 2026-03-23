//! Prompt version control with diff and rollback.
//!
//! Provides [`PromptRepository`] for committing, checking out, diffing, and
//! rolling back versioned prompt content using a line-by-line LCS diff
//! (Wagner-Fischer algorithm).

/// A single committed version of a prompt.
#[derive(Debug, Clone, PartialEq)]
pub struct PromptVersion {
    /// Monotonically increasing version number starting at 1.
    pub version: u32,
    /// Full prompt content at this version.
    pub content: String,
    /// Author identifier (name or email).
    pub author: String,
    /// Unix timestamp (seconds since epoch) when the commit was created.
    pub created_at: u64,
    /// Human-readable commit message.
    pub message: String,
    /// Version number of the parent commit, or `None` for the initial commit.
    pub parent_version: Option<u32>,
}

/// Line-by-line diff between two prompt versions.
#[derive(Debug, Clone, PartialEq)]
pub struct PromptDiff {
    /// Lines present in `b` but not in `a`.
    pub added_lines: Vec<String>,
    /// Lines present in `a` but not in `b`.
    pub removed_lines: Vec<String>,
    /// Number of lines common to both versions.
    pub unchanged_lines: usize,
}

impl PromptDiff {
    /// Returns `true` when there are no additions or removals.
    pub fn is_empty(&self) -> bool {
        self.added_lines.is_empty() && self.removed_lines.is_empty()
    }

    /// Short human-readable summary: `"+N -M ~K"`.
    pub fn summary(&self) -> String {
        format!(
            "+{} -{} ~{}",
            self.added_lines.len(),
            self.removed_lines.len(),
            self.unchanged_lines
        )
    }
}

/// Compute a line-by-line diff of two strings using the Wagner-Fischer LCS algorithm.
///
/// Returns a [`PromptDiff`] describing lines added, removed, and unchanged.
pub fn diff_prompts(a: &str, b: &str) -> PromptDiff {
    let a_lines: Vec<&str> = a.lines().collect();
    let b_lines: Vec<&str> = b.lines().collect();

    let m = a_lines.len();
    let n = b_lines.len();

    // Build the LCS table (Wagner-Fischer).
    let mut dp = vec![vec![0usize; n + 1]; m + 1];
    for i in 1..=m {
        for j in 1..=n {
            if a_lines[i - 1] == b_lines[j - 1] {
                dp[i][j] = dp[i - 1][j - 1] + 1;
            } else {
                dp[i][j] = dp[i - 1][j].max(dp[i][j - 1]);
            }
        }
    }

    // Back-track to classify each line.
    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut unchanged = 0usize;

    let mut i = m;
    let mut j = n;
    while i > 0 || j > 0 {
        if i > 0 && j > 0 && a_lines[i - 1] == b_lines[j - 1] {
            unchanged += 1;
            i -= 1;
            j -= 1;
        } else if j > 0 && (i == 0 || dp[i][j - 1] >= dp[i - 1][j]) {
            added.push(b_lines[j - 1].to_string());
            j -= 1;
        } else {
            removed.push(a_lines[i - 1].to_string());
            i -= 1;
        }
    }

    // Reverse so lines appear in document order.
    added.reverse();
    removed.reverse();

    PromptDiff {
        added_lines: added,
        removed_lines: removed,
        unchanged_lines: unchanged,
    }
}

/// An append-only store of prompt versions with diff, rollback, and blame support.
#[derive(Debug, Default)]
pub struct PromptRepository {
    versions: Vec<PromptVersion>,
    next_version: u32,
}

impl PromptRepository {
    /// Create an empty repository.
    pub fn new() -> Self {
        Self {
            versions: Vec::new(),
            next_version: 1,
        }
    }

    /// Commit new content and return the assigned version number.
    pub fn commit(&mut self, content: &str, author: &str, message: &str) -> u32 {
        let version = self.next_version;
        let parent_version = if version > 1 { Some(version - 1) } else { None };
        self.versions.push(PromptVersion {
            version,
            content: content.to_string(),
            author: author.to_string(),
            created_at: current_timestamp(),
            message: message.to_string(),
            parent_version,
        });
        self.next_version += 1;
        version
    }

    /// Return a reference to the [`PromptVersion`] with the given number, if it exists.
    pub fn checkout(&self, version: u32) -> Option<&PromptVersion> {
        self.versions
            .iter()
            .find(|v| v.version == version)
    }

    /// Compute the diff between two committed versions.
    ///
    /// Returns `None` if either version does not exist.
    pub fn diff(&self, from: u32, to: u32) -> Option<PromptDiff> {
        let a = self.checkout(from)?;
        let b = self.checkout(to)?;
        Some(diff_prompts(&a.content, &b.content))
    }

    /// Return all versions in chronological (ascending version number) order.
    pub fn log(&self) -> Vec<&PromptVersion> {
        self.versions.iter().collect()
    }

    /// Create a new commit whose content matches `to_version`, effectively
    /// reverting to that state. Returns the new version number, or `None` if
    /// `to_version` does not exist.
    pub fn rollback(&mut self, to_version: u32) -> Option<u32> {
        let target = self.checkout(to_version)?;
        let content = target.content.clone();
        let message = format!("Rollback to version {}", to_version);
        let new_version = self.commit(&content, "system", &message);
        Some(new_version)
    }

    /// Return the version that last introduced (or modified) the given 0-based
    /// line index in the current HEAD content.
    ///
    /// Iterates versions in reverse order and returns the first version where
    /// the specified line differs from its parent.  Returns `None` if the
    /// repository is empty or the line index is out of range.
    pub fn blame(&self, line: usize) -> Option<&PromptVersion> {
        if self.versions.is_empty() {
            return None;
        }

        // Walk versions from newest to oldest.
        for idx in (0..self.versions.len()).rev() {
            let version = &self.versions[idx];
            let lines: Vec<&str> = version.content.lines().collect();
            if line >= lines.len() {
                continue;
            }
            if idx == 0 {
                // Initial commit — this version introduced the line.
                return Some(version);
            }
            let parent = &self.versions[idx - 1];
            let parent_lines: Vec<&str> = parent.content.lines().collect();
            let parent_line = parent_lines.get(line).copied().unwrap_or("");
            if lines[line] != parent_line {
                return Some(version);
            }
        }
        // Line unchanged since the first commit.
        self.versions.first()
    }

    /// Return all versions committed by `author`, in chronological order.
    pub fn history_for_author<'a>(&'a self, author: &str) -> Vec<&'a PromptVersion> {
        self.versions
            .iter()
            .filter(|v| v.author == author)
            .collect()
    }
}

/// Returns the current Unix timestamp in seconds.
///
/// Uses `std::time::SystemTime` so there is no external dependency.
fn current_timestamp() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── diff_prompts ──────────────────────────────────────────────────────────

    #[test]
    fn diff_identical_is_empty() {
        let d = diff_prompts("hello\nworld", "hello\nworld");
        assert!(d.is_empty());
        assert_eq!(d.unchanged_lines, 2);
    }

    #[test]
    fn diff_empty_strings() {
        let d = diff_prompts("", "");
        assert!(d.is_empty());
        assert_eq!(d.unchanged_lines, 0);
    }

    #[test]
    fn diff_all_added() {
        let d = diff_prompts("", "line1\nline2");
        assert_eq!(d.added_lines, vec!["line1", "line2"]);
        assert!(d.removed_lines.is_empty());
    }

    #[test]
    fn diff_all_removed() {
        let d = diff_prompts("line1\nline2", "");
        assert_eq!(d.removed_lines, vec!["line1", "line2"]);
        assert!(d.added_lines.is_empty());
    }

    #[test]
    fn diff_single_line_change() {
        let d = diff_prompts("hello\nworld", "hello\nearth");
        assert_eq!(d.removed_lines, vec!["world"]);
        assert_eq!(d.added_lines, vec!["earth"]);
        assert_eq!(d.unchanged_lines, 1);
    }

    #[test]
    fn diff_summary_format() {
        let d = diff_prompts("a\nb\nc", "a\nd\ne\nf");
        // "b" and "c" removed; "d", "e", "f" added; "a" unchanged
        let summary = d.summary();
        assert!(summary.starts_with('+'));
        assert!(summary.contains('-'));
        assert!(summary.contains('~'));
    }

    // ── PromptRepository ─────────────────────────────────────────────────────

    #[test]
    fn commit_returns_sequential_versions() {
        let mut repo = PromptRepository::new();
        assert_eq!(repo.commit("v1", "alice", "init"), 1);
        assert_eq!(repo.commit("v2", "bob", "update"), 2);
        assert_eq!(repo.commit("v3", "alice", "fix"), 3);
    }

    #[test]
    fn checkout_existing_version() {
        let mut repo = PromptRepository::new();
        repo.commit("content one", "alice", "first");
        let v = repo.checkout(1).expect("version 1 must exist");
        assert_eq!(v.content, "content one");
        assert_eq!(v.author, "alice");
        assert_eq!(v.parent_version, None);
    }

    #[test]
    fn checkout_missing_version_is_none() {
        let repo = PromptRepository::new();
        assert!(repo.checkout(99).is_none());
    }

    #[test]
    fn parent_version_set_correctly() {
        let mut repo = PromptRepository::new();
        repo.commit("v1", "alice", "init");
        repo.commit("v2", "alice", "update");
        assert_eq!(repo.checkout(2).unwrap().parent_version, Some(1));
    }

    #[test]
    fn diff_between_versions() {
        let mut repo = PromptRepository::new();
        repo.commit("line1\nline2", "alice", "init");
        repo.commit("line1\nline3", "bob", "change");
        let d = repo.diff(1, 2).expect("diff must exist");
        assert_eq!(d.removed_lines, vec!["line2"]);
        assert_eq!(d.added_lines, vec!["line3"]);
    }

    #[test]
    fn diff_missing_version_is_none() {
        let mut repo = PromptRepository::new();
        repo.commit("v1", "a", "m");
        assert!(repo.diff(1, 99).is_none());
    }

    #[test]
    fn log_is_chronological() {
        let mut repo = PromptRepository::new();
        repo.commit("a", "x", "1");
        repo.commit("b", "x", "2");
        repo.commit("c", "x", "3");
        let log = repo.log();
        assert_eq!(log.len(), 3);
        assert_eq!(log[0].version, 1);
        assert_eq!(log[2].version, 3);
    }

    #[test]
    fn rollback_creates_new_commit_with_old_content() {
        let mut repo = PromptRepository::new();
        repo.commit("original content", "alice", "init");
        repo.commit("changed content", "bob", "oops");
        let new_ver = repo.rollback(1).expect("rollback must succeed");
        assert_eq!(new_ver, 3);
        let v = repo.checkout(3).unwrap();
        assert_eq!(v.content, "original content");
        assert!(v.message.contains('1'));
    }

    #[test]
    fn rollback_missing_version_is_none() {
        let mut repo = PromptRepository::new();
        assert!(repo.rollback(42).is_none());
    }

    #[test]
    fn blame_initial_commit() {
        let mut repo = PromptRepository::new();
        repo.commit("line0\nline1", "alice", "init");
        let v = repo.blame(0).expect("blame must return a version");
        assert_eq!(v.version, 1);
    }

    #[test]
    fn blame_changed_line_points_to_later_version() {
        let mut repo = PromptRepository::new();
        repo.commit("line0\nline1", "alice", "init");
        repo.commit("line0\nchanged", "bob", "update line1");
        let v = repo.blame(1).expect("blame must return a version");
        assert_eq!(v.version, 2);
    }

    #[test]
    fn blame_out_of_range_returns_none() {
        let mut repo = PromptRepository::new();
        repo.commit("one line", "alice", "init");
        assert!(repo.blame(999).is_none());
    }

    #[test]
    fn history_for_author_filters_correctly() {
        let mut repo = PromptRepository::new();
        repo.commit("a", "alice", "1");
        repo.commit("b", "bob", "2");
        repo.commit("c", "alice", "3");
        let alice_history = repo.history_for_author("alice");
        assert_eq!(alice_history.len(), 2);
        assert!(alice_history.iter().all(|v| v.author == "alice"));
    }

    #[test]
    fn history_for_unknown_author_is_empty() {
        let mut repo = PromptRepository::new();
        repo.commit("a", "alice", "1");
        assert!(repo.history_for_author("nobody").is_empty());
    }
}
