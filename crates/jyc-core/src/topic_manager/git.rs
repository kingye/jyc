//! Git helpers: branch detection and changed-file diffing for topic dirs.
//!
//! Extracted from the monolithic `topic_manager.rs`.

use std::path::Path;

use jyc_types::{ChangeKind, ChangedFileEntry};

/// Locate a checkout's git metadata: the working dir to run `git` in and the
/// on-disk `HEAD` file to read the branch from.
///
/// Handles three layouts:
/// - a plain clone: `<path>/.git` is a directory and
///   `HEAD` is `<path>/.git/HEAD`;
/// - a linked worktree or submodule: `<path>/.git` is a *file* containing
///   `gitdir: <dir>`, and `HEAD` lives in that dir (a relative `gitdir`
///   resolves against `<path>`);
/// - the agent's clone-into-`repo/` layout.
///
/// No `git` CLI for the probe itself — `.git/HEAD` is git's stable on-disk
/// format and `std::fs::read_to_string` follows a `repo/` symlink for us.
fn git_probe(path: &Path) -> Option<(std::path::PathBuf, std::path::PathBuf)> {
    let dotgit = path.join(".git");
    if dotgit.join("HEAD").is_file() {
        return Some((path.to_path_buf(), dotgit.join("HEAD")));
    }
    if dotgit.is_file() {
        let content = std::fs::read_to_string(&dotgit).ok()?;
        let gitdir = content.trim().strip_prefix("gitdir:")?.trim();
        let gitdir = if Path::new(gitdir).is_absolute() {
            std::path::PathBuf::from(gitdir)
        } else {
            path.join(gitdir)
        };
        let head = gitdir.join("HEAD");
        if head.is_file() {
            return Some((path.to_path_buf(), head));
        }
    }
    let repo = path.join("repo");
    if repo.join(".git").join("HEAD").is_file() {
        return Some((repo.clone(), repo.join(".git").join("HEAD")));
    }
    None
}

/// Read the current branch name from `.git/HEAD` under `path`.
///
/// Returns:
/// - `Some(branch)` for a symbolic ref `ref: refs/heads/<branch>`
/// - `Some("(detached)")` for a raw SHA in `.git/HEAD`
/// - `None` when no git metadata is found (not a git repo, perms, etc.)
pub(crate) fn branch_for_topic_path(path: &Path) -> Option<String> {
    let (_, head_path) = git_probe(path)?;
    let raw = std::fs::read_to_string(&head_path).ok()?;
    let trimmed = raw.trim();
    if let Some(rest) = trimmed.strip_prefix("ref: refs/heads/") {
        if rest.is_empty() || rest.contains('\n') || rest.contains(' ') {
            return None;
        }
        Some(rest.to_string())
    } else if trimmed.len() == 40 && trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
        Some("(detached)".to_string())
    } else {
        None
    }
}

/// Run `git diff --name-only <revspec>` in `cwd` and return the trimmed
/// stdout lines, or `None` on spawn / non-zero exit / non-UTF8.
fn run_git_diff(cwd: &Path, revspec: &str) -> Option<Vec<String>> {
    let output = std::process::Command::new("git")
        .args(["diff", "--name-only", revspec])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(String::from)
            .collect(),
    )
}

/// Run `git diff --name-status <revspec>` and parse each line as
/// `(status_letter, path)`. Returns `None` on spawn / non-zero exit /
/// non-UTF8, mirroring [`run_git_diff`]. Renames (`R<score><TAB>old<TAB>new`),
/// copies (`C<score><TAB>src<TAB>dst`), and type changes (`T<TAB>path`)
/// are normalized to `ChangeKind::Modified` — the chat info pane only
/// distinguishes the three primary statuses (Added / Modified / Deleted),
/// and emitting a `Renamed` variant would need a separate renderer
/// surface (YAGNI for now). For renames / copies, the LAST
/// tab-separated field is the post-rename path; the old name is dropped.
fn run_git_diff_name_status(cwd: &Path, revspec: &str) -> Option<Vec<(ChangeKind, String)>> {
    let output = std::process::Command::new("git")
        .args(["diff", "--name-status", revspec])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let mut out = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        // `<status>\t<path>` for most kinds, `<status>\t<old>\t<new>` for renames/copies.
        let mut fields = line.split('\t');
        let Some(status) = fields.next() else {
            continue;
        };
        let kind = match status.chars().next() {
            Some('A') => ChangeKind::Added,
            Some('D') => ChangeKind::Deleted,
            _ => ChangeKind::Modified,
        };
        // `last()` is the new path for renames/copies, or just the path
        // for the common case. Skip empty lines defensively.
        let Some(path) = fields.next_back() else {
            continue;
        };
        if path.is_empty() {
            continue;
        }
        out.push((kind, path.to_string()));
    }
    Some(out)
}

/// Resolve the base ref to diff the topic branch against.
///
/// Probes in order: `origin/HEAD` (set by clone to the remote's default
/// branch), then local `main`, then local `master`. Returns the first
/// ref that resolves, or `None` when none do — e.g. a repo whose
/// default branch has another name and no origin remote.
fn base_ref(cwd: &Path) -> Option<String> {
    for candidate in ["origin/HEAD", "main", "master"] {
        // `.output()` (not `.status()`) so the child's stdout is captured:
        // a successful `rev-parse` prints the resolved SHA, which an
        // inherited stdout would leak into jyc's own output.
        let out = std::process::Command::new("git")
            .args(["rev-parse", "--verify", "--quiet", candidate])
            .current_dir(cwd)
            .output()
            .ok()?;
        if out.status.success() {
            return Some(candidate.to_string());
        }
    }
    None
}

/// List files changed relative to the repo's default branch, with
/// per-file `uncommitted` flag and `change` kind.
///
/// Runs two `git diff` invocations against the topic's working directory:
/// 1. `git diff --name-status <base>...HEAD` — files committed on the
///    branch vs the default branch (resolved by [`base_ref`]), with
///    status letter parsed into [`ChangeKind`].
/// 2. `git diff --name-only HEAD` — files modified in the working tree
///    but not yet committed (staged + unstaged); kind defaults to
///    `Modified` since these are tracked files that exist in HEAD.
///
/// The two are unioned by path; any path in the dirty list gets
/// `uncommitted: true`. A path appearing in both is emitted once with the
/// branch-side kind and `uncommitted: true` (the more-noisy state wins
/// for the flag; kind is branch-side because the dirty-in-tree state
/// alone doesn't reveal whether the file was Added / Modified /
/// Deleted on the branch).
///
/// Mirrors [`branch_for_topic_path`]: looks first at `<path>/.git/HEAD`,
/// then falls back at `<path>/repo/.git/HEAD`. Returns:
///
/// - `None` when neither `.git/HEAD` exists (not a git repo).
/// - `None` when BOTH `git` invocations fail (missing binary, no
///   resolvable base ref, etc.). If one succeeds, the successful one
///   still contributes its files.
/// - `Some(vec![])` when both diffs come back empty.
/// - `Some(vec![{path, change, uncommitted}, ...])` for the union,
///   sorted alphabetically by path.
///
/// Synchronous `std::process::Command` to match `branch_for_topic_path`'s
/// style.
pub(crate) fn changed_files_for_topic_path(path: &Path) -> Option<Vec<ChangedFileEntry>> {
    // `git` runs in the checkout root; the CLI itself resolves a worktree's
    // file-typed `.git`, so only the probe needs to know that layout.
    let (cwd, _) = git_probe(path)?;

    let branch =
        base_ref(&cwd).and_then(|base| run_git_diff_name_status(&cwd, &format!("{base}...HEAD")));
    let dirty = run_git_diff(&cwd, "HEAD");

    // Skip rule: not a git repo at all is the only path to `None`. If
    // either diff produces output (or an empty Vec) we have something
    // to ship.
    if branch.is_none() && dirty.is_none() {
        return None;
    }

    let mut map: std::collections::HashMap<String, (ChangeKind, bool)> =
        std::collections::HashMap::new();
    if let Some(branch) = branch {
        for (kind, path) in branch {
            map.insert(path, (kind, false));
        }
    }
    if let Some(dirty) = dirty {
        for path in dirty {
            map.entry(path)
                .and_modify(|(_, uncommitted)| *uncommitted = true)
                .or_insert((ChangeKind::Modified, true));
        }
    }
    let mut out: Vec<ChangedFileEntry> = map
        .into_iter()
        .map(|(path, (change, uncommitted))| ChangedFileEntry {
            path,
            change,
            uncommitted,
        })
        .collect();
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Some(out)
}

#[cfg(test)]
mod branch_resolution_tests {
    use super::branch_for_topic_path;
    use tempfile::tempdir;

    #[test]
    fn reads_symbolic_ref() {
        let dir = tempdir().unwrap();
        let git = dir.path().join(".git");
        std::fs::create_dir_all(&git).unwrap();
        std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        assert_eq!(branch_for_topic_path(dir.path()).as_deref(), Some("main"));
    }

    #[test]
    fn reads_detached_head() {
        let dir = tempdir().unwrap();
        let git = dir.path().join(".git");
        std::fs::create_dir_all(&git).unwrap();
        std::fs::write(git.join("HEAD"), "0123456789abcdef0123456789abcdef01234567").unwrap();
        assert_eq!(
            branch_for_topic_path(dir.path()).as_deref(),
            Some("(detached)")
        );
    }

    #[test]
    fn returns_none_when_no_git() {
        let dir = tempdir().unwrap();
        assert!(branch_for_topic_path(dir.path()).is_none());
    }

    #[test]
    fn follows_repo_subdir_layout() {
        let dir = tempdir().unwrap();
        let repo = dir.path().join("repo");
        let git = repo.join(".git");
        std::fs::create_dir_all(&git).unwrap();
        std::fs::write(git.join("HEAD"), "ref: refs/heads/feature-x\n").unwrap();
        assert_eq!(
            branch_for_topic_path(dir.path()).as_deref(),
            Some("feature-x")
        );
    }

    #[test]
    fn returns_none_on_garbage_head() {
        let dir = tempdir().unwrap();
        let git = dir.path().join(".git");
        std::fs::create_dir_all(&git).unwrap();
        std::fs::write(git.join("HEAD"), "garbage content\n").unwrap();
        assert!(branch_for_topic_path(dir.path()).is_none());
    }

    #[test]
    fn follows_worktree_gitfile_with_absolute_gitdir() {
        let dir = tempdir().unwrap();
        let gitdir = dir.path().join("admin-dir");
        std::fs::create_dir_all(&gitdir).unwrap();
        std::fs::write(gitdir.join("HEAD"), "ref: refs/heads/wt\n").unwrap();
        std::fs::write(
            dir.path().join(".git"),
            format!("gitdir: {}\n", gitdir.display()),
        )
        .unwrap();
        assert_eq!(branch_for_topic_path(dir.path()).as_deref(), Some("wt"));
    }

    #[test]
    fn follows_worktree_gitfile_with_relative_gitdir() {
        let dir = tempdir().unwrap();
        // gitdir sits next to the checkout; the pointer is relative to the
        // checkout dir, as `git worktree add` writes it when the admin dir
        // is addressed relatively.
        let gitdir = dir.path().with_file_name(format!(
            "{}-admin",
            dir.path().file_name().unwrap().to_string_lossy()
        ));
        std::fs::create_dir_all(&gitdir).unwrap();
        std::fs::write(gitdir.join("HEAD"), "ref: refs/heads/wt\n").unwrap();
        let relative = format!(
            "../{}-admin\n",
            gitdir.file_name().unwrap().to_string_lossy()
        );
        std::fs::write(dir.path().join(".git"), format!("gitdir: {relative}")).unwrap();
        assert_eq!(branch_for_topic_path(dir.path()).as_deref(), Some("wt"));
    }

    #[test]
    fn ignores_malformed_gitfile() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join(".git"), "not a gitdir pointer\n").unwrap();
        assert!(branch_for_topic_path(dir.path()).is_none());
    }
}

#[cfg(test)]
mod changed_files_resolution_tests {
    use super::{branch_for_topic_path, changed_files_for_topic_path};
    use jyc_types::{ChangeKind, ChangedFileEntry};
    use std::process::Command;
    use tempfile::tempdir;

    /// Init a git repo with a `main` branch and an initial empty commit.
    /// Returns the tempdir; caller is responsible for keeping it alive
    /// (TempDir drops at end of the test function).
    fn git_init_with_main() -> tempfile::TempDir {
        git_init_with_branch("main")
    }

    /// Init a git repo with the given default branch and an initial
    /// empty commit.
    fn git_init_with_branch(branch: &str) -> tempfile::TempDir {
        let dir = tempdir().unwrap();
        let run = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .expect("git failed")
        };
        // Ensure deterministic branch name + committer across CI hosts.
        run(&["init", "-q", "-b", branch]);
        run(&[
            "-c",
            "user.email=test@example.com",
            "-c",
            "user.name=Test",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "init",
        ]);
        dir
    }

    /// A linked worktree (`.git` is a file pointing at the admin dir) is a
    /// valid topic dir: the branch resolves from the pointed-at HEAD and
    /// `git diff` runs with the checkout as cwd. Regression: `/spawn` into a
    /// `git worktree add` dir showed neither branch nor files in topic info.
    #[test]
    fn changed_files_and_branch_work_in_a_git_worktree() {
        let repo = git_init_with_main();
        let run = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(repo.path())
                .output()
                .expect("git failed")
        };
        std::fs::write(repo.path().join("a.txt"), "base\n").unwrap();
        run(&["add", "a.txt"]);
        run(&[
            "-c",
            "user.email=test@example.com",
            "-c",
            "user.name=Test",
            "commit",
            "-q",
            "-m",
            "add a",
        ]);

        let wt = tempdir().unwrap();
        run(&[
            "-c",
            "user.email=test@example.com",
            "-c",
            "user.name=Test",
            "worktree",
            "add",
            "-q",
            "-b",
            "wt-branch",
            wt.path().to_str().unwrap(),
        ]);
        assert_eq!(
            branch_for_topic_path(wt.path()).as_deref(),
            Some("wt-branch")
        );

        std::fs::write(wt.path().join("a.txt"), "changed\n").unwrap();
        let files =
            changed_files_for_topic_path(wt.path()).expect("a worktree should probe as a git repo");
        assert_eq!(
            files,
            vec![ChangedFileEntry {
                path: "a.txt".to_string(),
                change: ChangeKind::Modified,
                uncommitted: true,
            }]
        );
    }

    #[test]
    fn returns_none_when_no_git() {
        let dir = tempdir().unwrap();
        assert!(changed_files_for_topic_path(dir.path()).is_none());
    }

    #[test]
    fn returns_empty_vec_when_branch_is_main() {
        let dir = git_init_with_main();
        // HEAD == main → diff main...HEAD is empty.
        let files = changed_files_for_topic_path(dir.path());
        assert_eq!(files, Some(vec![]));
    }

    #[test]
    fn lists_files_against_master_default_branch() {
        let dir = git_init_with_branch("master");
        let run = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .expect("git failed")
        };
        run(&["checkout", "-q", "-b", "feature"]);
        std::fs::write(dir.path().join("alpha.rs"), "fn a() {}").unwrap();
        run(&["add", "alpha.rs"]);
        run(&[
            "-c",
            "user.email=t@e",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            "alpha",
        ]);

        let files = changed_files_for_topic_path(dir.path()).expect("git diff must run");
        assert_eq!(
            files,
            vec![ChangedFileEntry {
                path: "alpha.rs".into(),
                uncommitted: false,
                change: ChangeKind::Added,
            }]
        );
    }

    #[test]
    fn prefers_origin_head_over_local_main() {
        let dir = git_init_with_main();
        let run = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .expect("git failed")
        };
        // A `master` branch that `main` does not contain, then point
        // origin/HEAD at it — the base probe must pick master, not main.
        run(&["checkout", "-q", "-b", "master"]);
        std::fs::write(dir.path().join("m.rs"), "fn m() {}").unwrap();
        run(&["add", "m.rs"]);
        run(&[
            "-c",
            "user.email=t@e",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            "m",
        ]);
        run(&[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/heads/master",
        ]);
        run(&["checkout", "-q", "-b", "feature"]);
        std::fs::write(dir.path().join("f.rs"), "fn f() {}").unwrap();
        run(&["add", "f.rs"]);
        run(&[
            "-c",
            "user.email=t@e",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            "f",
        ]);

        // Only the feature-branch commit is vs master; vs main this
        // would also list m.rs.
        let files = changed_files_for_topic_path(dir.path()).expect("git diff must run");
        assert_eq!(
            files,
            vec![ChangedFileEntry {
                path: "f.rs".into(),
                uncommitted: false,
                change: ChangeKind::Added,
            }]
        );
    }

    #[test]
    fn falls_back_to_dirty_only_when_no_base_resolves() {
        let dir = git_init_with_branch("develop");
        let run = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .expect("git failed")
        };
        std::fs::write(dir.path().join("tracked.rs"), "v1").unwrap();
        run(&["add", "tracked.rs"]);
        run(&[
            "-c",
            "user.email=t@e",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            "add",
        ]);
        // Uncommitted modification; no main/master/origin-HEAD exists,
        // so the branch diff is skipped and only the dirty diff reports.
        std::fs::write(dir.path().join("tracked.rs"), "v2").unwrap();

        let files = changed_files_for_topic_path(dir.path()).expect("git diff must run");
        assert_eq!(
            files,
            vec![ChangedFileEntry {
                path: "tracked.rs".into(),
                uncommitted: true,
                change: ChangeKind::Modified,
            }]
        );
    }

    #[test]
    fn lists_files_committed_on_feature_branch() {
        let dir = git_init_with_main();
        let run = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .expect("git failed")
        };
        // Create a feature branch with two commits touching distinct files.
        run(&["checkout", "-q", "-b", "feature"]);
        std::fs::write(dir.path().join("alpha.rs"), "fn a() {}").unwrap();
        run(&["add", "alpha.rs"]);
        run(&[
            "-c",
            "user.email=t@e",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            "alpha",
        ]);
        std::fs::write(dir.path().join("beta.rs"), "fn b() {}").unwrap();
        run(&["add", "beta.rs"]);
        run(&[
            "-c",
            "user.email=t@e",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            "beta",
        ]);

        let files = changed_files_for_topic_path(dir.path()).expect("git diff must run");
        assert_eq!(
            files,
            vec![
                ChangedFileEntry {
                    path: "alpha.rs".into(),
                    uncommitted: false,
                    change: ChangeKind::Added,
                },
                ChangedFileEntry {
                    path: "beta.rs".into(),
                    uncommitted: false,
                    change: ChangeKind::Added,
                },
            ]
        );
    }

    #[test]
    fn follows_repo_subdir_layout() {
        // Shared-repo layout: topic dir contains a `repo/` subdir that
        // holds the actual git working tree (see `branch_for_topic_path`).
        let outer = tempdir().unwrap();
        let repo = outer.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let run = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(&repo)
                .output()
                .expect("git failed")
        };
        run(&["init", "-q", "-b", "main"]);
        run(&[
            "-c",
            "user.email=t@e",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "init",
        ]);
        run(&["checkout", "-q", "-b", "feature"]);
        std::fs::write(repo.join("gamma.rs"), "fn g() {}").unwrap();
        run(&["add", "gamma.rs"]);
        run(&[
            "-c",
            "user.email=t@e",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            "gamma",
        ]);

        let files = changed_files_for_topic_path(outer.path()).expect("must read repo/");
        assert_eq!(
            files,
            vec![ChangedFileEntry {
                path: "gamma.rs".into(),
                uncommitted: false,
                change: ChangeKind::Added,
            }]
        );
    }

    #[test]
    fn returns_some_empty_when_only_dirty_diff_runs() {
        // Repo exists with only a non-main branch — `main...HEAD`
        // fails, but `git diff --name-only HEAD` still runs (HEAD
        // exists from `git_init_with_main`'s empty commit) and is
        // empty because the tree is clean. The function must NOT
        // collapse this to `None` — that would hide the entire
        // section just because `main` is missing.
        let dir = git_init_with_main();
        let run = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .expect("git failed")
        };
        // Drop `main` so the branch-diff fails, but keep HEAD valid.
        run(&["branch", "-D", "main"]);
        assert_eq!(
            changed_files_for_topic_path(dir.path()),
            Some(vec![]),
            "missing main must not collapse to None when HEAD diff succeeds"
        );
    }

    #[test]
    fn lists_uncommitted_only_files() {
        // Feature branch, tracked-but-not-committed file → `git diff HEAD`
        // surfaces it as uncommitted, `main...HEAD` is empty.
        let dir = git_init_with_main();
        let run = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .expect("git failed")
        };
        run(&["checkout", "-q", "-b", "feature"]);
        std::fs::write(dir.path().join("draft.rs"), "fn d() {}").unwrap();
        // Stage but don't commit — leaves it tracked + dirty.
        run(&["add", "draft.rs"]);

        let files = changed_files_for_topic_path(dir.path()).expect("git diff must run");
        assert_eq!(
            files,
            vec![ChangedFileEntry {
                path: "draft.rs".into(),
                uncommitted: true,
                change: ChangeKind::Modified,
            }]
        );
    }

    #[test]
    fn promotes_committed_to_uncommitted_when_dirty() {
        // Path committed earlier on the branch AND edited again since.
        // The single entry must have `uncommitted: true` (more-noisy wins).
        let dir = git_init_with_main();
        let run = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .expect("git failed")
        };
        run(&["checkout", "-q", "-b", "feature"]);
        std::fs::write(dir.path().join("shared.rs"), "fn s() {}\n").unwrap();
        run(&["add", "shared.rs"]);
        run(&[
            "-c",
            "user.email=t@e",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            "shared",
        ]);
        // Now edit the same path again — leaves it dirty in the tree.
        std::fs::write(dir.path().join("shared.rs"), "fn s() { /*x*/ }\n").unwrap();

        let files = changed_files_for_topic_path(dir.path()).expect("git diff must run");
        assert_eq!(files.len(), 1, "must dedupe to one entry");
        assert_eq!(files[0].path, "shared.rs");
        assert!(files[0].uncommitted, "more-noisy state must win");
    }

    #[test]
    fn lists_deleted_files() {
        // Path deleted on the branch (status `D` from `git diff
        // --name-status main...HEAD`). The entry must surface with
        // `change: ChangeKind::Deleted`.
        let dir = git_init_with_main();
        let run = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .expect("git failed")
        };
        // Seed `main` with a tracked file so there's something to delete.
        std::fs::write(dir.path().join("doomed.rs"), "fn d() {}\n").unwrap();
        run(&["add", "doomed.rs"]);
        run(&[
            "-c",
            "user.email=t@e",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            "seed",
        ]);
        // Branch off, delete the file, commit.
        run(&["checkout", "-q", "-b", "feature"]);
        run(&["rm", "-q", "doomed.rs"]);
        run(&[
            "-c",
            "user.email=t@e",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            "kill",
        ]);

        let files = changed_files_for_topic_path(dir.path()).expect("git diff must run");
        assert_eq!(
            files,
            vec![ChangedFileEntry {
                path: "doomed.rs".into(),
                uncommitted: false,
                change: ChangeKind::Deleted,
            }]
        );
    }

    #[test]
    fn renames_label_as_modified() {
        // `git mv` produces `R100<TAB>old<TAB>new` in
        // `git diff --name-status`. Our parser takes the LAST tab field
        // (the new path) and labels the kind as `Modified` (per YAGNI
        // — no separate `Renamed` variant).
        let dir = git_init_with_main();
        let run = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .expect("git failed")
        };
        // Seed `main` with a tracked file.
        std::fs::write(dir.path().join("original.rs"), "fn o() {}\n").unwrap();
        run(&["add", "original.rs"]);
        run(&[
            "-c",
            "user.email=t@e",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            "seed",
        ]);
        // Rename on the branch.
        run(&["checkout", "-q", "-b", "feature"]);
        run(&["mv", "original.rs", "renamed.rs"]);
        run(&[
            "-c",
            "user.email=t@e",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            "rename",
        ]);

        let files = changed_files_for_topic_path(dir.path()).expect("git diff must run");
        assert_eq!(
            files,
            vec![ChangedFileEntry {
                path: "renamed.rs".into(),
                uncommitted: false,
                change: ChangeKind::Modified,
            }],
            "rename must surface under the new path with kind=Modified"
        );
    }

    #[test]
    fn dirty_only_paths_label_as_modified() {
        // Path only in `git diff --name-only HEAD` (not committed on
        // the branch). Kind defaults to `Modified` since the file
        // exists in HEAD and was edited locally.
        let dir = git_init_with_main();
        let run = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .expect("git failed")
        };
        // Seed `main` with a tracked file.
        std::fs::write(dir.path().join("existing.rs"), "fn e() {}\n").unwrap();
        run(&["add", "existing.rs"]);
        run(&[
            "-c",
            "user.email=t@e",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            "seed",
        ]);
        // Edit locally without committing.
        std::fs::write(dir.path().join("existing.rs"), "fn e() { /*x*/ }\n").unwrap();

        let files = changed_files_for_topic_path(dir.path()).expect("git diff must run");
        assert_eq!(
            files,
            vec![ChangedFileEntry {
                path: "existing.rs".into(),
                uncommitted: true,
                change: ChangeKind::Modified,
            }],
            "dirty-only path: kind=Modified (it exists in HEAD), uncommitted=true"
        );
    }
}
