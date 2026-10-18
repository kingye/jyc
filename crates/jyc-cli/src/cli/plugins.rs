//! `jyc plugins` — install a plugin archive into the config dir.
//!
//! A plugin archive is the `dist/jyc-plugins.tar.gz` the release workflow
//! builds out of this repo: a gzipped tar holding `skills/`, `subagents/` and
//! `hooks/` at its root. A plain directory laid out the same way is accepted
//! too, so a bundle can be installed before it is ever packed.
//!
//! Every collection the source carries is installed into `<target>/<name>/`
//! (default: the platform config home, e.g. `~/.config/jyc/`): `skills/` and
//! `subagents/` entry by entry, one subdirectory per name and each replaced
//! cleanly, and `hooks/` file by file — its entries are flat shell scripts, not
//! one directory per name. A collection
//! the source does not carry is skipped, and a directory this command does
//! not know (e.g. a `scripts/` that only makes sense in the repo) is named in
//! a printed line and left alone. Unlike `skills/` and `subagents/`, `hooks/`
//! is not picked up by a directory scan: a `[[hooks]]` entry in `config.toml`
//! has to name the installed script.

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use std::path::{Path, PathBuf};

use super::install;

/// The collections a plugin archive may carry, as `<name>` under the root.
const COLLECTIONS: &[&str] = &["skills", "subagents", "hooks"];

/// The one collection whose entries are flat files rather than one directory
/// per name, and which is therefore installed file by file.
const FLAT_COLLECTION: &str = "hooks";

/// Actions for the `plugins` subcommand.
#[derive(Debug, Subcommand)]
pub enum PluginsAction {
    /// Install a plugin archive (.tar.gz) or a directory laid out like it
    Install {
        /// Plugin archive (.tar.gz), or a directory holding the same collections
        source: PathBuf,

        /// Target directory (default: platform config home, e.g. ~/.config/jyc)
        #[arg(long)]
        target: Option<PathBuf>,
    },
}

/// Dispatch to the appropriate subcommand handler.
pub async fn run(action: &PluginsAction) -> Result<()> {
    match action {
        PluginsAction::Install { source, target } => {
            let source = install::resolve_source(source);
            let target = install::resolve_target(target.as_deref())?;
            install_plugins(&source, &target).await
        }
    }
}

/// Install the collections a plugin source carries into `target`.
///
/// `source` is either a directory laid out as an archive root or a `.tar.gz`
/// holding one; the archive is unpacked into a temporary directory that lives
/// until the install is done.
pub async fn install_plugins(source: &Path, target: &Path) -> Result<()> {
    guard_source_and_target(source, target)?;

    // `Root` keeps the unpacked directory alive for as long as it is borrowed.
    let root = open_root(source)?;
    let root_path = root.path();

    let mut installed = 0usize;
    let mut entries = tokio::fs::read_dir(root_path)
        .await
        .with_context(|| format!("failed to read {}", root_path.display()))?;
    while let Some(entry) = entries.next_entry().await? {
        // `file_type` does not follow symlinks (unlike `Path::is_dir`), so a
        // member that merely points at a directory is not installed.
        if !entry.file_type().await?.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if !COLLECTIONS.contains(&name.as_str()) {
            // Command output, not logging: the same channel `install_entries`
            // prints its "installed <name>" lines to.
            println!("skipping unknown plugin collection: {name}/");
            continue;
        }
        let path = entry.path();
        // Every collection but `hooks/` is one subdirectory per name, replaced
        // cleanly. `hooks/` holds flat shell scripts instead, so it is
        // copied file by file: `install_entries` enumerates subdirectories and
        // would find none.
        if name == FLAT_COLLECTION {
            let copied =
                jyc_core::template_utils::overwrite_template_files(&path, &target.join(&name))
                    .await?;
            println!("installed {name}/ ({copied} file(s))");
        } else {
            install::install_entries(root_path, target, &name, None).await?;
        }
        installed += 1;
    }

    if installed == 0 {
        bail!(
            "no plugin collections ({}) found in {} — a bundle holds them at its root",
            COLLECTIONS.join(", "),
            source.display()
        );
    }
    Ok(())
}

/// The unpacked plugin source, and the temporary directory backing it.
enum Root {
    /// The source was already a directory — borrowed as it is.
    Dir(PathBuf),
    /// The source was an archive — unpacked here, so it outlives the install.
    Archive(tempfile::TempDir),
}

impl Root {
    fn path(&self) -> &Path {
        match self {
            Root::Dir(path) => path,
            Root::Archive(dir) => dir.path(),
        }
    }
}

/// Take the plugin root out of `source`, unpacking an archive if that is what
/// it is. Blocking on purpose: the archive is local and small.
fn open_root(source: &Path) -> Result<Root> {
    if source.is_dir() {
        return Ok(Root::Dir(source.to_path_buf()));
    }

    let file = std::fs::File::open(source)
        .with_context(|| format!("failed to open plugin source {}", source.display()))?;
    let dir = tempfile::tempdir().context("failed to create a temporary directory")?;
    tar::Archive::new(flate2::read::GzDecoder::new(file))
        .unpack(dir.path())
        .with_context(|| {
            format!(
                "failed to unpack {} (a plugin archive is a gzipped tar: .tar.gz)",
                source.display()
            )
        })?;
    Ok(Root::Archive(dir))
}

/// Refuse the two shapes that would lose data: a source that is the target
/// (installing `~/.config/jyc` onto itself — an entry is removed before it is
/// copied, so the source would disappear with it), and a target inside the
/// source (the copy would walk into what it is writing, duplicating the
/// source into itself rather than deleting it).
fn guard_source_and_target(source: &Path, target: &Path) -> Result<()> {
    let src = source
        .canonicalize()
        .with_context(|| format!("plugin source does not exist: {}", source.display()))?;
    let dst = resolve_through_existing_ancestors(target);

    if src == dst {
        bail!(
            "source and target are the same directory ({}) — install a bundle, not the config dir",
            src.display()
        );
    }
    if dst.starts_with(&src) {
        bail!(
            "install target must be outside the plugin source ({} is inside {})",
            dst.display(),
            src.display()
        );
    }
    Ok(())
}

/// `target` resolved as far as it exists: canonicalize its deepest existing
/// ancestor — which resolves symlinks and `..` — then re-append the part that
/// does not exist yet. A bare `std::path::absolute` would leave a symlinked
/// ancestor unresolved and let a target that really is inside the source pass
/// the guard (the target is usually created by the install itself, so it
/// cannot simply be canonicalized).
fn resolve_through_existing_ancestors(target: &Path) -> PathBuf {
    let mut rest = Vec::new();
    let mut current = target;
    loop {
        if let Ok(mut resolved) = current.canonicalize() {
            for part in rest.iter().rev() {
                resolved.push(part);
            }
            return resolved;
        }
        match (current.file_name(), current.parent()) {
            (Some(name), Some(parent)) => {
                rest.push(name);
                current = parent;
            }
            _ => {
                return std::path::absolute(target).unwrap_or_else(|_| target.to_path_buf());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Lay out a plugin tree under `root` the way an archive root looks.
    fn write_fixture(root: &Path) {
        for (dir, file) in [
            ("skills/demo", "SKILL.md"),
            ("subagents/role", "config.toml"),
            ("hooks", "bell.sh"),
        ] {
            let path = root.join(dir);
            std::fs::create_dir_all(&path).unwrap();
            let mut f = std::fs::File::create(path.join(file)).unwrap();
            writeln!(f, "demo").unwrap();
        }
    }

    /// Pack `root`'s three collections into a gzipped tar at `archive`.
    fn write_archive(root: &Path, archive: &Path) {
        let file = std::fs::File::create(archive).unwrap();
        let enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut builder = tar::Builder::new(enc);
        for name in COLLECTIONS {
            builder
                .append_dir_all(name, root.join(name))
                .unwrap_or_else(|e| panic!("failed to append {name}/: {e}"));
        }
        builder.into_inner().unwrap().finish().unwrap();
    }

    /// The names of the collections under `target`, sorted.
    fn installed(target: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(target)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[tokio::test]
    async fn installs_every_collection_from_a_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("bundle");
        write_fixture(&source);
        let target = tmp.path().join("config");

        install_plugins(&source, &target).await.unwrap();

        assert_eq!(installed(&target), ["hooks", "skills", "subagents"]);
        assert!(target.join("skills/demo/SKILL.md").is_file());
        assert!(target.join("subagents/role/config.toml").is_file());
        assert!(target.join("hooks/bell.sh").is_file());
    }

    #[tokio::test]
    async fn installs_every_collection_from_an_archive() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("bundle");
        write_fixture(&source);
        let archive = tmp.path().join("jyc-plugins.tar.gz");
        write_archive(&source, &archive);
        let target = tmp.path().join("config");

        install_plugins(&archive, &target).await.unwrap();

        assert!(target.join("skills/demo/SKILL.md").is_file());
        assert!(target.join("subagents/role/config.toml").is_file());
        assert!(target.join("hooks/bell.sh").is_file());
    }

    #[tokio::test]
    async fn installs_only_the_collections_the_source_carries() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("bundle");
        std::fs::create_dir_all(source.join("skills/demo")).unwrap();
        std::fs::write(source.join("skills/demo/SKILL.md"), "demo").unwrap();
        let target = tmp.path().join("config");

        install_plugins(&source, &target).await.unwrap();

        assert_eq!(installed(&target), ["skills"]);
    }

    #[tokio::test]
    async fn ignores_collection_directories_it_does_not_know() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("bundle");
        write_fixture(&source);
        // `scripts/` belongs to the repo, not to a plugin bundle.
        std::fs::create_dir_all(source.join("scripts")).unwrap();
        std::fs::write(source.join("scripts/deploy.sh"), "demo").unwrap();
        let target = tmp.path().join("config");

        install_plugins(&source, &target).await.unwrap();

        assert_eq!(installed(&target), ["hooks", "skills", "subagents"]);
    }

    #[tokio::test]
    async fn refuses_a_source_with_no_known_collection() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("bundle");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("README.md"), "nothing to install").unwrap();
        let target = tmp.path().join("config");

        let err = install_plugins(&source, &target).await.unwrap_err();
        assert!(
            err.to_string().contains("no plugin collections"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn refuses_a_target_inside_the_source() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("bundle");
        write_fixture(&source);
        let target = source.join("installed");

        let err = install_plugins(&source, &target).await.unwrap_err();
        assert!(
            err.to_string().contains("outside the plugin source"),
            "unexpected error: {err}"
        );
        // The guard runs before anything is written, so the source is intact.
        assert!(!target.exists());
        assert!(source.join("skills/demo/SKILL.md").is_file());
        assert!(source.join("hooks/bell.sh").is_file());
    }

    #[tokio::test]
    async fn refuses_the_source_installed_onto_itself() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("config");
        write_fixture(&source);

        let err = install_plugins(&source, &source).await.unwrap_err();
        assert!(
            err.to_string().contains("same directory"),
            "unexpected error: {err}"
        );
        assert!(source.join("skills/demo/SKILL.md").is_file());
    }

    #[tokio::test]
    async fn overwrites_an_installed_entry_without_dropping_others() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("bundle");
        write_fixture(&source);
        let target = tmp.path().join("config");
        std::fs::create_dir_all(target.join("skills/mine")).unwrap();
        std::fs::write(target.join("skills/mine/SKILL.md"), "mine").unwrap();

        install_plugins(&source, &target).await.unwrap();

        assert!(target.join("skills/demo/SKILL.md").is_file());
        // An entry the bundle does not carry is left in place.
        assert!(target.join("skills/mine/SKILL.md").is_file());
    }

    #[tokio::test]
    async fn overwrites_a_hook_file_and_keeps_the_others() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("bundle");
        write_fixture(&source);
        let target = tmp.path().join("config");
        std::fs::create_dir_all(target.join("hooks")).unwrap();
        std::fs::write(target.join("hooks/bell.sh"), "old").unwrap();
        std::fs::write(target.join("hooks/mine.sh"), "mine").unwrap();

        install_plugins(&source, &target).await.unwrap();

        // The bundle's version of a hook replaces the installed one...
        assert_eq!(
            std::fs::read_to_string(target.join("hooks/bell.sh")).unwrap(),
            "demo\n"
        );
        // ...while a hook the bundle does not carry is kept.
        assert_eq!(
            std::fs::read_to_string(target.join("hooks/mine.sh")).unwrap(),
            "mine"
        );
    }

    #[tokio::test]
    async fn refuses_a_source_that_is_not_an_archive() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("bundle.tar.gz");
        std::fs::write(&source, "not a gzipped tar").unwrap();
        let target = tmp.path().join("config");

        let err = install_plugins(&source, &target).await.unwrap_err();
        assert!(
            err.to_string().contains("gzipped tar"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn refuses_a_source_that_does_not_exist() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("missing");
        let target = tmp.path().join("config");

        let err = install_plugins(&source, &target).await.unwrap_err();
        assert!(
            err.to_string().contains("does not exist"),
            "unexpected error: {err}"
        );
    }

    /// The target does not exist yet, but its *ancestor* resolves inside the
    /// source through a symlink. Resolving only the target itself would miss
    /// it, so the guard has to resolve the deepest ancestor that exists.
    #[cfg(unix)]
    #[tokio::test]
    async fn refuses_a_target_inside_the_source_through_a_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("bundle");
        write_fixture(&source);
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&source, &link).unwrap();
        let target = link.join("installed");

        let err = install_plugins(&source, &target).await.unwrap_err();
        assert!(
            err.to_string().contains("outside the plugin source"),
            "unexpected error: {err}"
        );
        assert!(!target.exists());
        assert!(source.join("skills/demo/SKILL.md").is_file());
    }
}
