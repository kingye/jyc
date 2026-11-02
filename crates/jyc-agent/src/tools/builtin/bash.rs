//! Bash tool — execute shell commands.
//!
//! ## Security boundary
//!
//! `bash` checks the paths a command names before running it. The command is
//! split into words the way bash splits it —
//! [`jyc_core::command::line::split_statements`], the same splitter the
//! dispatcher uses (plus the statement boundary a newline draws), so
//! `cat "/srv/My Files"` arrives as one word rather than three — each
//! path-shaped word is turned into an absolute path, and every one of those goes
//! through `ToolContext::check_write_boundary`. Relative words resolve against
//! the directory the command starts in, moved along by the `cd` targets the
//! command spells out at command position — `cd a; cd ..; cat x`, a `cd` on its
//! own line, and `if cd a; then …` all count — so `cd repo && cat ../../secret`
//! is judged against the directory it will actually read from rather than
//! against the process's own.
//!
//! **Limitations**: this is a lexical check, not a sandbox. A path that only
//! exists at run time is invisible to it — a `$VAR` whose value the command does
//! not assign itself, `$(...)`, a path inside a string handed to another program
//! (`python3 -c 'open(...)'`), a `cd` whose target is not a literal word — and so
//! is a destination that creates its file unless a `>` or a `cd` names it by
//! position. Closing that class needs OS-level enforcement (Landlock on Linux,
//! seatbelt on macOS); see issue #1047.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::process::Command;
use tracing;

use super::super::{Tool, ToolContext, ToolOutput, normalize};

/// Maximum output size before truncation (128KB).
const MAX_OUTPUT_SIZE: usize = 128 * 1024;

/// Default timeout for bash commands (2 minutes).
const DEFAULT_TIMEOUT_SECS: u64 = 120;

/// Bash tool — executes shell commands in the working directory.
pub struct BashTool;

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Execute a bash command in the working directory. Use for tests, builds, git, \
         file operations and any system command."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string"
                },
                "timeout": {
                    "type": "integer",
                    "description": "Optional timeout in seconds (default: 120)"
                }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput> {
        let command = input
            .get("command")
            .and_then(|c| c.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'command' parameter"))?;

        let timeout_secs = input
            .get("timeout")
            .and_then(|t| t.as_u64())
            .unwrap_or(DEFAULT_TIMEOUT_SECS);

        // Boundary check: every path the command names has to be inside the
        // working directory, the configured read roots, or the write roots.
        for named in judged_paths(command, ctx) {
            if let Err(msg) =
                ctx.check_write_boundary(&named.path.display().to_string(), &named.path)
            {
                return Ok(ToolOutput::error(msg));
            }
        }

        tracing::debug!(command = %command, timeout = timeout_secs, "Executing bash command");

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(timeout_secs),
            Command::new("bash")
                .arg("-c")
                .arg(command)
                .current_dir(ctx.working_dir)
                .envs(git_ignore_env())
                .output(),
        )
        .await;

        match result {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);

                let mut result_text = String::new();

                if !stdout.is_empty() {
                    result_text.push_str(&stdout);
                }
                if !stderr.is_empty() {
                    if !result_text.is_empty() {
                        result_text.push('\n');
                    }
                    result_text.push_str("STDERR:\n");
                    result_text.push_str(&stderr);
                }

                // Truncate if too large
                if result_text.len() > MAX_OUTPUT_SIZE {
                    result_text.truncate(MAX_OUTPUT_SIZE);
                    result_text.push_str("\n... [output truncated]");
                }

                if result_text.is_empty() {
                    result_text = format!(
                        "Command completed with exit code {}",
                        output.status.code().unwrap_or(-1)
                    );
                }

                let is_error = !output.status.success();
                if is_error {
                    let exit_code = output.status.code().unwrap_or(-1);
                    result_text = format!("Exit code: {}\n{}", exit_code, result_text);
                }

                Ok(if is_error {
                    ToolOutput::error(result_text)
                } else {
                    ToolOutput::success(result_text)
                })
            }
            Ok(Err(e)) => Ok(ToolOutput::error(format!("Failed to execute command: {e}"))),
            Err(_) => Ok(ToolOutput::error(format!(
                "Command timed out after {}s",
                timeout_secs
            ))),
        }
    }
}

/// Name of the global git excludes file, written inside the JYC data dir
/// (outside every repo).
const GIT_IGNORE_GLOBAL_FILENAME: &str = "git-ignore-global";

/// Environment variables injected into every bash command so that **any**
/// `git` invocation inside the tool ignores JYC's runtime `.jyc/` directory
/// — regardless of which repo the command runs in, and without writing
/// anything into the repo (no `.gitignore`, no `.git/info/exclude`, no
/// visible trace for collaborators).
///
/// Works by pointing `core.excludesfile` at a global excludes file living
/// under the JYC data dir, via `GIT_CONFIG_*` env vars — git config
/// injected this way has the highest precedence, above system/global/local
/// config files.
///
/// Tradeoff: a user-level `core.excludesfile` in `~/.gitconfig` is
/// overridden inside bash-tool git commands.
/// `ponytail:` no merge support — append the user's existing excludes file
/// later, only if that scenario is ever observed in practice.
fn git_ignore_env() -> Vec<(&'static str, String)> {
    match jyc_utils::paths::data_dir() {
        Some(dir) => git_ignore_env_in(&dir),
        None => Vec::new(),
    }
}

/// `git_ignore_env()` for an explicit data directory — the file is created
/// on first use. Kept separate from [`git_ignore_env`] so tests can point
/// it at a tempdir instead of the real data home.
fn git_ignore_env_in(data_dir: &Path) -> Vec<(&'static str, String)> {
    let excludes = data_dir.join(GIT_IGNORE_GLOBAL_FILENAME);
    if !excludes.exists() {
        let _ = std::fs::create_dir_all(data_dir);
        let _ = std::fs::write(&excludes, ".jyc/\n");
    }
    vec![
        ("GIT_CONFIG_COUNT", "1".to_string()),
        ("GIT_CONFIG_KEY_0", "core.excludesfile".to_string()),
        ("GIT_CONFIG_VALUE_0", excludes.display().to_string()),
    ]
}

/// A path a command names, and how the command names it.
struct NamedPath {
    path: PathBuf,
    /// Named by position — a `cd` target or a redirection target — so it is a
    /// path even when nothing is there yet. An ordinary word has to resolve to
    /// something on disk first, which is what keeps the `/etc/foo` inside
    /// `--body "fix /etc/foo"` from being read as a path.
    by_position: bool,
}

/// Every path `command` names, in the order they appear.
///
/// Words come from [`jyc_core::command::line::split`] — the splitter the
/// dispatcher itself uses — so quoting is already resolved and a word holding a
/// space arrives whole. A word names a path when it holds a `/`, starts with
/// `~`, or is a `cd` or redirection target (a path by position); the `=` in
/// `--path=/srv/x` separates a flag from a path, as it does for the program
/// being run.
///
/// Relative words resolve against `working_dir`, moved along by each `cd` the
/// command spells out. A `cd` whose target cannot be read as a literal path
/// (`cd "$X"` for a variable the command never assigned, `cd ~user`, `cd -`, a
/// bare `cd`) makes the directory unknown: absolute words are still checked
/// after it, relative ones are not, because what they name is no longer
/// visible here.
///
/// A `NAME=value` word at command position assigns; it reads nothing, so the
/// path its value spells out is not judged — `X=/usr; echo hi` runs. The value
/// is remembered instead, and a later word that starts with `$NAME` or
/// `${NAME}` is judged as the value: `X=/usr; cd $X` is refused for the
/// directory it really enters. An assignment-shaped word anywhere else is an
/// argument the program reads (`dd if=/etc/passwd`), so it is judged as before.
///
/// Two shapes of "the shell does not do what the words look like" are honoured
/// rather than guessed at, both by matching the word the line spells and never
/// what a variable expands to: expansion re-creates neither shell syntax nor an
/// assignment, so `X=then; $X cd sub` runs a command named `then` and
/// `A=X=1; $A cd sub` runs one named `X=1`, leaving the directory alone. A
/// `cd` inside `( … )` runs in a subshell and does not move the directory the
/// rest of the line runs in, whatever the parentheses are glued to.
fn named_paths(command: &str, working_dir: &Path) -> Vec<NamedPath> {
    // An unclosed quote is a syntax error to bash too: a command bash refuses
    // to run needs no boundary check. Statements are split at newlines as well
    // as at `;`, so a `cd` at the start of a later line is seen as a `cd`.
    let Ok(words) = jyc_core::command::line::split_statements(command) else {
        return Vec::new();
    };

    let mut paths = Vec::new();
    let mut cwd = Some(working_dir.to_path_buf());
    let mut at_command_start = true;
    let mut redirect_target = false;
    // How many `( … )` the words have opened and not closed yet. A `cd` inside
    // one runs in a subshell and never moves the shell the rest of the line runs
    // in, so tracking it would judge what follows against a directory the
    // command is not in — the one direction that fails *open*.
    let mut subshell_depth = 0usize;
    // The literal assignments seen so far, so `X=…; cd $X` names the directory
    // the command really enters.
    let mut vars: HashMap<String, String> = HashMap::new();
    let mut words = words.iter();

    while let Some(raw) = words.next() {
        // Counted from the word the line spells: quoting is gone by now, so a
        // literal `(` (`echo "("`) counts as a grouping one. That only leaves
        // the tracked directory shallower than the shell's, which judges a
        // relative `..` word further out than the command reads it — never
        // further in.
        subshell_depth = group_depth(subshell_depth, raw);
        let expanded = shell_word(raw, &vars);
        let word = expanded.as_str();
        if word.is_empty() {
            // `&&`, `||`, `;`, `|`, `(` … — the next word starts a command.
            at_command_start = true;
            redirect_target = false;
            continue;
        }

        // `>`, `2>>`, `&>`: what follows is a file the command writes. A tail
        // holding a variable is expanded like any other word — `cat x 2>$X`
        // writes where `$X` points, and the spaced form (`2> $X`) already was.
        if let Some((_, tail)) = leading_redirection(word) {
            let tail = expand_vars(tail, &vars);
            if tail.is_empty() {
                redirect_target = true;
            } else if looks_like_path(&tail)
                && let Some(path) = resolve_path(&tail, cwd.as_deref())
            {
                paths.push(NamedPath {
                    path,
                    by_position: true,
                });
            }
            continue;
        }
        let by_position = std::mem::take(&mut redirect_target);

        // `cd` moves the directory the rest of the command resolves against,
        // so its target is a path even when it holds no separator (`cd repo`).
        if at_command_start && word == "cd" {
            // Where the `cd` itself sits — before the target's own parentheses
            // are folded in, because `(cd sub)` closes the group on the target
            // word and that `cd` still ran inside it.
            let cd_depth = subshell_depth;
            let raw_target = words.next();
            // The target is taken straight from the iterator, so its own
            // parentheses do not pass the loop top.
            if let Some(raw_target) = raw_target {
                subshell_depth = group_depth(subshell_depth, raw_target);
            }
            let target = raw_target.map(|word| shell_word(word, &vars));

            // `cd 2> err` sends stderr to a file: the directory is the one a
            // bare `cd` goes to, which the line does not spell out, so later
            // relative words are not judged. The file is still a write.
            if let Some((_, tail)) = target.as_deref().and_then(leading_redirection) {
                let tail = expand_vars(tail, &vars);
                if cd_depth == 0 {
                    cwd = None;
                }
                if tail.is_empty() {
                    redirect_target = true;
                } else if looks_like_path(&tail)
                    && let Some(path) = resolve_path(&tail, None)
                {
                    paths.push(NamedPath {
                        path,
                        by_position: true,
                    });
                }
                at_command_start = raw_target.map(String::as_str).is_none_or(starts_command);
                continue;
            }

            let entered = match target.as_deref() {
                Some(target) if !target.is_empty() && target != "-" => {
                    resolve_path(target, cwd.as_deref())
                }
                // A bare `cd` goes to `$HOME`; `cd -` to wherever that was.
                _ => None,
            };
            // The path is one the command really enters, in whichever shell, so
            // it is judged either way. Only the directory the *rest of the line*
            // resolves against is left alone inside `( … )`: that `cd` never
            // reaches the shell running the words after it.
            if cd_depth == 0 {
                cwd = entered.clone();
            }
            if let Some(path) = entered {
                paths.push(NamedPath {
                    path,
                    by_position: true,
                });
            }
            // The target can end the statement itself — `cd a; cd ..` glues the
            // separator to the target word — and then the next word is again at
            // command position. Dropping that is an escape: the second `cd` goes
            // untracked and `..` is judged one directory too shallow.
            at_command_start = raw_target.map(String::as_str).is_none_or(starts_command);
            continue;
        }

        // `NAME=value` at command position assigns; it reads nothing, so the
        // path its value spells out is not judged — `X=/usr; echo hi` and
        // `PATH=/usr/local/bin:$PATH cmd` run. Reading the value as a path was a
        // false refusal, and one that only ever caught the spelling `X=/usr`,
        // never `X=/u\sr`. The value is kept instead, so a later `$NAME` is
        // judged as what it really names.
        //
        // Only at command position: anywhere else the word is an argument the
        // program interprets, and `dd if=/etc/passwd` really reads that file —
        // `if` is a shell name, so skipping it there would open the boundary.
        //
        // Matched against the word the line spells, not the value a variable
        // expands to: bash never re-reads an expansion as an assignment, so
        // `A=X=1; $A cd sub` runs a command named `X=1` and stays where it is.
        // Reading that expansion as an assignment handed command position to
        // the `cd` beside it and moved the tracked directory — the same escape
        // as reading one as a keyword.
        if !by_position
            && at_command_start
            && let Some((name, value)) = assignment(trim_shell_punctuation(raw))
        {
            vars.insert(name.to_string(), value.to_string());
            // The words after an assignment are still at command position: more
            // assignments (`X=1 X=2 cmd`) and the command name that follows are
            // all read the same way, and a separator glued to the value
            // (`X=1; cd …`) opens a statement either way. A `cd` inside
            // `( … )` is held back by `subshell_depth`, not by dropping command
            // position here — which only ever covered the `(` glued to the
            // assignment (`(X=1`), and not the spaced spelling `( X=1`.
            at_command_start = true;
            continue;
        }

        // A keyword opens a command position only where a command could start;
        // `echo if cd sub` prints three words and `if` there is not the reserved
        // word. Letting it through would read `cd sub` as a `cd` the shell never
        // runs, shift the tracked directory, and judge what follows against a
        // directory the command does not enter — the one direction that fails
        // *open*.
        let was_command_start = at_command_start;
        // Matched against the word the line spells, not the value a variable
        // expands to: bash resolves reserved words before expansion, so
        // `X=then; $X cd sub` runs a command named `then` and the `cd` beside it
        // is an argument. Reading the expansion as `then` would move the tracked
        // directory into `sub` and judge the words behind it too shallow.
        at_command_start = starts_command(raw)
            || (was_command_start && COMMAND_KEYWORDS.contains(&trim_shell_punctuation(raw)));
        if looks_like_path(word) {
            // `--path=/srv/x` separates a flag from the path it names. A word
            // that is itself a path keeps its `=`: `/tmp/a=b` is one file, and
            // splitting it would check `/tmp/a` — a path the command never
            // touches — while missing the one it does.
            let named = match word.split_once('=') {
                Some((flag, value)) if !looks_like_path(flag) => value,
                _ => word,
            };
            if let Some(path) = resolve_path(named, cwd.as_deref()) {
                paths.push(NamedPath { path, by_position });
            }
        }
    }
    paths
}

/// The paths `command` names that the boundary judges at all.
///
/// An ordinary word that names nothing on disk is not a path the command can
/// reach: it is a flag value or data (`--body "fix /etc/foo"`). A target named
/// by position (`cd /x`, `> /x`) is one whether or not it is there yet — the
/// only way to catch a write that creates its file.
///
/// [`execute`] and the tests share this gate, so a test cannot keep passing on
/// a rule the tool no longer applies.
fn judged_paths(command: &str, ctx: &ToolContext<'_>) -> Vec<NamedPath> {
    named_paths(command, ctx.working_dir)
        .into_iter()
        .filter(|named| named.by_position || named.path.exists())
        .collect()
}

/// Words that put the next word at command position without being a command
/// themselves, so `if cd a; then cat b; fi` tracks the `cd` like `cd a; cat b`
/// does.
const COMMAND_KEYWORDS: [&str; 7] = ["if", "while", "until", "then", "do", "elif", "else"];

/// Whether the word after `raw` starts a new command — a separator can be glued
/// to the end of a word (`done;`, `x&&`, `y|`).
fn starts_command(raw: &str) -> bool {
    raw.ends_with([';', '&', '|'])
}

/// `depth` after `word`, counting the `( … )` groups it opens and closes.
///
/// The splitter only breaks on whitespace, so a group can open or close on a
/// word that is also something else: `(X=1` opens one, `sub)` closes one. Both
/// spellings of `( cd sub )` — glued or spaced — therefore move the depth.
fn group_depth(depth: usize, word: &str) -> usize {
    depth
        .saturating_add(word.matches('(').count())
        .saturating_sub(word.matches(')').count())
}

/// `word` without the shell punctuation that can sit against it: a separator
/// glued to a path (`cd /tmp;`, `cat x|head`) or a grouping parenthesis
/// (`(cd /tmp)`).
///
/// Only the ends are trimmed, and redirection operators are left to
/// [`leading_redirection`], which knows the target is a path.
fn trim_shell_punctuation(word: &str) -> &str {
    word.trim_matches(['(', ')', '{', '}', ';', '&', '|', '!'])
}

/// The redirection operator `word` leads with, and what follows it.
///
/// `>` and `2>` come back with an empty tail (the target is the next word),
/// `>/tmp/f` with `/tmp/f`.
fn leading_redirection(word: &str) -> Option<(&str, &str)> {
    let prefix = word
        .find(|ch: char| !ch.is_ascii_digit() && ch != '&')
        .unwrap_or(word.len());
    let rest = &word[prefix..];
    let operator = ["<<<", "<<", ">>", "<>", ">|", ">", "<"]
        .into_iter()
        .find(|operator| rest.starts_with(operator))?;
    let split = prefix + operator.len();
    Some((&word[..split], &word[split..]))
}

/// Whether `word` names a path rather than a flag or data: it holds a
/// separator, or starts at the home directory.
fn looks_like_path(word: &str) -> bool {
    word.contains('/') || word.starts_with('~')
}

/// One word the shell would run: `raw` with its leading variables expanded from
/// `vars`, then the punctuation a separator or a grouping parenthesis glues to
/// it trimmed off.
///
/// In that order, because `${X};` ends with a brace the trim would take for
/// grouping punctuation — expanding first leaves the value, and the `;` after it
/// is trimmed as any glued separator is.
fn shell_word(raw: &str, vars: &HashMap<String, String>) -> String {
    trim_shell_punctuation(&expand_vars(raw, vars)).to_string()
}

/// The `NAME=value` assignment `word` spells, if it is one.
///
/// The name has to be a shell name — a leading letter or `_`, then letters,
/// digits, `_` — which is what tells `X=/usr` (an assignment) from
/// `--path=/usr` (a flag carrying a path, which the program being run reads).
fn assignment(word: &str) -> Option<(&str, &str)> {
    let (name, value) = word.split_once('=')?;
    let mut chars = name.chars();
    let first = chars.next()?;
    if !(first.is_ascii_alphabetic() || first == '_')
        || !chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    {
        return None;
    }
    Some((name, value))
}

/// `word` with a leading `$NAME` or `${NAME}` replaced by `vars`'s value for it.
///
/// Only the leading run is expanded: `$X/foo` and a whole `$X` are what commands
/// spell out for a directory or a file, while `a$X` and `${X%%…}` are left as
/// they are — [`resolve_path`] then declines the word, which is the same answer
/// this pass gave before it knew any variable at all.
///
/// Quoting is already gone when a word gets here, so `'$X'` expands too. The
/// shell reads a literal `$X` there and this pass judges the assigned value
/// instead — the wrong answer in the safe direction, and the same one it gives
/// `cat '/etc/passwd'`, which is judged on purpose.
fn expand_vars(word: &str, vars: &HashMap<String, String>) -> String {
    let mut rest = word;
    let mut expanded = String::new();
    while let Some(after_dollar) = rest.strip_prefix('$') {
        let (name, after) = match after_dollar.strip_prefix('{') {
            Some(braced) => match braced.split_once('}') {
                Some((name, after)) => (name, after),
                None => break,
            },
            None => {
                let end = after_dollar
                    .find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
                    .unwrap_or(after_dollar.len());
                (&after_dollar[..end], &after_dollar[end..])
            }
        };
        let Some(value) = vars.get(name) else { break };
        expanded.push_str(value);
        rest = after;
    }
    expanded.push_str(rest);
    expanded
}

/// `word` as an absolute path, or `None` when it cannot be read as one.
fn resolve_path(word: &str, cwd: Option<&Path>) -> Option<PathBuf> {
    // A word the shell still has to expand is not a path this pass can see.
    if word.contains('$') || word.contains('`') {
        return None;
    }
    let expanded = expand_home(word)?;
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        cwd?.join(expanded)
    };
    let normalized = normalize(&absolute);
    // A glob expands after this check: `cat /etc/shado*` names `/etc/shado*`
    // here and `/etc/shadow` to bash. Judging the directory the pattern sits in
    // is as close as a pass over the words can come.
    match normalized.parent() {
        Some(parent) if word.contains(['*', '?', '[']) => Some(parent.to_path_buf()),
        _ => Some(normalized),
    }
}

/// `word` with a leading `~` expanded. `None` for `~user`, whose home this pass
/// cannot name.
fn expand_home(word: &str) -> Option<PathBuf> {
    if word == "~" {
        return std::env::var_os("HOME").map(PathBuf::from);
    }
    match word.strip_prefix("~/") {
        Some(rest) => Some(PathBuf::from(std::env::var_os("HOME")?).join(rest)),
        None if word.starts_with('~') => None,
        None => Some(PathBuf::from(word)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The paths `command` names, as strings, when it runs in `working`.
    fn named(command: &str, working: &Path) -> Vec<String> {
        named_paths(command, working)
            .iter()
            .map(|named| named.path.display().to_string())
            .collect()
    }

    /// The paths `command` names that the boundary refuses, after the same
    /// existence gate `execute` applies (shared through [`judged_paths`]).
    ///
    /// A tempdir working directory is itself inside the always-allowed temp
    /// root, so every refusal fixture has to sit outside it: `/etc/passwd`
    /// exists, while `/jyc-no-such-root/…` cannot.
    fn refused(command: &str, working: &Path) -> Vec<String> {
        let ctx = ToolContext::new(working);
        judged_paths(command, &ctx)
            .into_iter()
            .filter(|named| {
                ctx.check_write_boundary(&named.path.display().to_string(), &named.path)
                    .is_err()
            })
            .map(|named| named.path.display().to_string())
            .collect()
    }

    /// `path` as the boundary sees it — canonical, so `/etc` is spelled the way
    /// the platform spells it.
    fn existing(path: &str) -> String {
        Path::new(path)
            .canonicalize()
            .unwrap_or_else(|_| Path::new(path).to_path_buf())
            .display()
            .to_string()
    }

    #[test]
    fn unquoted_absolute_path_detected() {
        let working = tempfile::tempdir().expect("create working dir");
        assert_eq!(
            refused("cat /etc/passwd", working.path()),
            [existing("/etc/passwd")]
        );
    }

    /// The blind spot this change closes: a path inside quotes used to be
    /// dropped, so `cat "/etc/passwd"` read it and `cat "/srv/My Files"` was
    /// never judged at all. Both are words like any other now.
    #[test]
    fn a_quoted_path_is_a_path() {
        let working = tempfile::tempdir().expect("create working dir");
        assert_eq!(
            refused(r#"cat "/etc/passwd""#, working.path()),
            [existing("/etc/passwd")]
        );
        assert_eq!(
            refused("cat '/etc/passwd'", working.path()),
            [existing("/etc/passwd")]
        );

        let spaced = "/jyc-no-such-root/My Files/notes.txt";
        assert_eq!(
            named(&format!(r#"cat "{spaced}""#), working.path()),
            [spaced]
        );
    }

    /// A path-like string that is *data* is not a refusal: it resolves inside
    /// the working directory, or it names nothing that exists.
    #[test]
    fn a_data_argument_is_not_a_path() {
        let working = tempfile::tempdir().expect("create working dir");
        for command in [
            r#"echo "// comment""#,
            r###"gh pr comment 273 --body "## Review\n\n### // comment\n\nDetails""###,
            r#"git commit -m "fix: docs/a.md and //""#,
            "cmd --note=/jyc-no-such-root/data",
        ] {
            assert_eq!(
                refused(command, working.path()),
                Vec::<String>::new(),
                "{command}"
            );
        }
    }

    /// A redirection target is a path by position: it counts whether or not
    /// anything is there yet, which is the only way a write that creates its
    /// file is caught. A descriptor and an operator (`2>/dev/null`, `2>&1`) are
    /// not a path, and `/dev/null` sits in the boundary because discarding
    /// output is not a way out of it.
    #[test]
    fn a_redirection_target_is_checked() {
        let working = tempfile::tempdir().expect("create working dir");
        for command in ["ls 2>/dev/null", "cat x >/dev/null", "cmd 2>&1"] {
            assert_eq!(
                refused(command, working.path()),
                Vec::<String>::new(),
                "{command}"
            );
        }
        assert_eq!(
            refused("cat x > /etc/passwd", working.path()),
            [existing("/etc/passwd")]
        );
        assert_eq!(
            refused("echo x > /jyc-no-such-root/new.txt", working.path()),
            ["/jyc-no-such-root/new.txt"]
        );
    }

    /// A separator glued to a word (`/tmp;`, `x|head`) is punctuation, not part
    /// of the path — otherwise every `cmd /path; next` would look outside.
    #[test]
    fn a_glued_separator_is_not_part_of_the_path() {
        let working = tempfile::tempdir().expect("create working dir");
        assert_eq!(
            refused("ls /tmp; echo done", working.path()),
            Vec::<String>::new()
        );
        assert_eq!(
            named("(cat /etc/passwd)", working.path()),
            [existing("/etc/passwd")]
        );
    }

    /// A relative path is judged against the directory the command will run in,
    /// not against the process's own — which is what makes `cd` matter.
    #[test]
    fn a_relative_path_resolves_against_the_tracked_directory() {
        let working = tempfile::tempdir().expect("create working dir");
        let root = working
            .path()
            .canonicalize()
            .expect("canonical working dir");
        let sub = root.join("sub");
        std::fs::create_dir(&sub).expect("create sub");

        assert_eq!(
            named("cd sub && cat ../notes.txt", working.path()),
            [
                sub.display().to_string(),
                root.join("notes.txt").display().to_string()
            ]
        );
    }

    /// `cd /` is refused, and the words after it are resolved where it pointed:
    /// the command never runs, so the escape behind it is moot — but the check
    /// names the path that would have been read.
    #[test]
    fn a_cd_target_is_checked() {
        let working = tempfile::tempdir().expect("create working dir");
        assert_eq!(
            refused("cd / && cat etc/passwd", working.path()),
            [existing("/"), existing("/etc/passwd")]
        );
        // The `cd` is what refuses this one: a bare word after it is only ever
        // resolved in a directory the command cannot reach.
        assert_eq!(
            refused("cd /etc && cat passwd", working.path()),
            [existing("/etc")]
        );
    }

    /// `..` used to be invisible — a word not starting with `/` was never a
    /// candidate — so `cat ../../.bashrc` read the real home directory.
    #[test]
    fn a_parent_escape_is_refused() {
        let working = tempfile::tempdir().expect("create working dir");
        assert_eq!(
            refused("cat ../../../../../../etc/passwd", working.path()),
            [existing("/etc/passwd")]
        );
    }

    /// `~` is the home directory to bash, so it is one here too.
    #[test]
    fn a_tilde_path_is_the_home_directory() {
        let working = tempfile::tempdir().expect("create working dir");
        let home = std::env::var_os("HOME").expect("HOME is set for the test");
        let home = PathBuf::from(home);
        let home = home.canonicalize().unwrap_or(home);
        assert_eq!(
            named("cat ~/.bashrc", working.path()),
            [home.join(".bashrc").display().to_string()]
        );
    }

    /// A glob expands after the check, so the pattern is judged by the
    /// directory it sits in: `cat /etc/shado*` reads `/etc/shadow` to bash.
    #[test]
    fn a_glob_is_judged_by_its_directory() {
        let working = tempfile::tempdir().expect("create working dir");
        assert_eq!(
            refused("cat /etc/shado*", working.path()),
            [existing("/etc")]
        );
    }

    /// A flag can carry its path after `=`; the program being run sees a path
    /// there, so this pass does too.
    #[test]
    fn a_path_after_an_equals_sign_is_a_path() {
        let working = tempfile::tempdir().expect("create working dir");
        assert_eq!(
            refused("cmd --conf=/etc/passwd", working.path()),
            [existing("/etc/passwd")]
        );
    }

    /// An assignment at command position reads nothing: `X=/usr` stores a
    /// string, and the word after it is the command. Judging the value refused
    /// both of these, and it only ever caught the spelling `X=/usr` —
    /// `X=/u\sr; cd $X` walked out anyway.
    #[test]
    fn an_assignment_is_not_a_read() {
        let working = tempfile::tempdir().expect("create working dir");
        for command in [
            "X=/usr; echo hi",
            "PATH=/usr/local/bin:$PATH cmd",
            "X=/usr printenv X",
        ] {
            assert_eq!(
                refused(command, working.path()),
                Vec::<String>::new(),
                "{command}"
            );
        }
    }

    /// A `NAME=value` argument is not an assignment — the program reads it, and
    /// `if` is a shell name, so skipping every such word would let
    /// `dd if=/etc/passwd` read outside the boundary.
    #[test]
    fn an_assignment_shaped_argument_is_still_judged() {
        let working = tempfile::tempdir().expect("create working dir");
        assert_eq!(
            refused("dd if=/etc/passwd of=/tmp/out", working.path()),
            [existing("/etc/passwd")]
        );
    }

    /// The value is judged where a later word uses it, so closing the assignment
    /// hole does not open this one: `X=/usr; cd $X` still lands in `/usr`.
    #[test]
    fn an_assigned_value_is_judged_where_it_is_used() {
        let working = tempfile::tempdir().expect("create working dir");
        // `./passwd` is what an ordinary word has to look like to be judged at
        // all, and it resolves to `/etc/passwd` only because the tracked `cd`
        // moved there — a plain `passwd` names no directory and is not a path.
        assert_eq!(
            refused("X=/etc; cd $X; cat ./passwd", working.path()),
            [existing("/etc"), existing("/etc/passwd")]
        );
        assert_eq!(
            refused("X=/etc/passwd; cat $X", working.path()),
            [existing("/etc/passwd")]
        );
        assert_eq!(
            refused("X=/etc/passwd; cat \"$X\"", working.path()),
            [existing("/etc/passwd")]
        );
        assert_eq!(
            refused("X=/etc/passwd; cat ${X}", working.path()),
            [existing("/etc/passwd")]
        );
    }

    /// A tracked value moves the directory as a literal target would, so the
    /// relative words after it are judged from where the command really is.
    #[test]
    fn a_tracked_cd_value_moves_the_directory() {
        let working = tempfile::tempdir().expect("create working dir");
        let root = working
            .path()
            .canonicalize()
            .expect("canonical working dir");
        let sub = root.join("sub");
        std::fs::create_dir(&sub).expect("create sub");

        assert_eq!(
            named("X=sub; cd $X; cat ../notes.txt", working.path()),
            [
                sub.display().to_string(),
                root.join("notes.txt").display().to_string()
            ]
        );

        // The command name right after an assignment is still at command
        // position, so `X=1 cd sub` really does move the directory.
        assert_eq!(
            named("X=1 cd sub; cat ../notes.txt", working.path()),
            [
                sub.display().to_string(),
                root.join("notes.txt").display().to_string()
            ]
        );
    }

    /// A chain of assignments before the command assigns every one of them:
    /// `X=1 X=2 cmd` runs `cmd` with both set, so the second is not an argument
    /// and `$X` names what it assigned.
    #[test]
    fn a_chain_of_assignments_all_assign() {
        let working = tempfile::tempdir().expect("create working dir");
        assert_eq!(
            refused("X=1 X=/etc/passwd cat $X", working.path()),
            [existing("/etc/passwd")]
        );
    }

    /// Reserved words are recognized before expansion: bash never re-reads an
    /// expanded word as a keyword, so `X=then; $X cd sub` runs a command named
    /// `then` and the `cd` beside it is an argument. Tracking it would move the
    /// directory into `sub` and judge `..` one level too shallow — the same
    /// escape as `echo if cd sub`, reached through a variable.
    #[test]
    fn an_expanded_keyword_does_not_open_a_command() {
        let working = tempfile::tempdir().expect("create working dir");
        std::fs::create_dir(working.path().join("sub")).expect("create sub");

        assert_eq!(
            refused("X=then; $X cd sub; cat ../../etc/passwd", working.path()),
            [existing("/etc/passwd")]
        );
    }

    /// A `cd` inside a subshell does not move the shell the rest of the line
    /// runs in, so the `(` that opened it does not leave command position
    /// behind for the assignment to hand on.
    #[test]
    fn an_assignment_in_a_subshell_does_not_move_the_directory() {
        let working = tempfile::tempdir().expect("create working dir");
        std::fs::create_dir(working.path().join("sub")).expect("create sub");

        assert_eq!(
            refused("(X=1 cd sub); cat ../../etc/passwd", working.path()),
            [existing("/etc/passwd")]
        );
    }

    /// The `(` need not be glued to the assignment: the splitter breaks on
    /// whitespace, so `( X=1 cd sub` reaches the assignment branch as a plain
    /// `X=1` word. Nothing about that leaves the subshell, so the `cd` beside it
    /// must not move the directory — the spelling base code refused only by
    /// accident, by dropping command position on the assignment.
    #[test]
    fn a_spaced_subshell_does_not_move_the_directory() {
        let working = tempfile::tempdir().expect("create working dir");
        let root = working
            .path()
            .canonicalize()
            .expect("canonical working dir");
        let sub = root.join("sub");
        std::fs::create_dir(&sub).expect("create sub");
        let outside = root.parent().expect("working dir has a parent");

        assert_eq!(
            refused("( X=1 cd sub); cat ../../etc/passwd", working.path()),
            [existing("/etc/passwd")]
        );
        // Not just the spelling with an assignment in front: the decision comes
        // from the depth, so `( cd sub )` on its own holds too. The `cd` target
        // is still a path the command enters; what must not move is where the
        // words after the `)` resolve from.
        assert_eq!(
            named("( cd sub ); cat ../notes.txt", working.path()),
            [
                sub.display().to_string(),
                outside.join("notes.txt").display().to_string()
            ]
        );
    }

    /// An expansion is no more an assignment than it is a keyword: bash runs a
    /// command named `X=1` for `$A`, so the `cd` beside it is an argument and
    /// the directory stays put. Reading the expansion as an assignment moved it
    /// into `sub`, where `..` judges one level too shallow.
    #[test]
    fn an_expanded_assignment_does_not_assign() {
        let working = tempfile::tempdir().expect("create working dir");
        let root = working
            .path()
            .canonicalize()
            .expect("canonical working dir");
        std::fs::create_dir(root.join("sub")).expect("create sub");
        let outside = root.parent().expect("working dir has a parent");

        assert_eq!(
            named("A=X=1; $A cd sub; cat ../notes.txt", working.path()),
            [outside.join("notes.txt").display().to_string()]
        );
    }

    /// A variable glued to the operator is the redirection target just as the
    /// spaced form is: `2>$X` writes where `$X` points.
    #[test]
    fn a_glued_redirection_target_is_expanded() {
        let working = tempfile::tempdir().expect("create working dir");
        assert_eq!(
            refused("X=/etc/passwd; cat note 2>$X", working.path()),
            [existing("/etc/passwd")]
        );
    }

    /// A `cd` this pass cannot read (`$X`) leaves the directory unknown: the
    /// relative word after it is not guessed at, while an absolute one is still
    /// checked.
    #[test]
    fn an_unreadable_cd_leaves_only_absolute_words() {
        let working = tempfile::tempdir().expect("create working dir");
        assert_eq!(
            named("cd $X && cat etc/passwd", working.path()),
            Vec::<String>::new()
        );
        assert_eq!(
            refused("cd $X && cat /etc/passwd", working.path()),
            [existing("/etc/passwd")]
        );
    }

    /// A `;` glued to the `cd` target ends the statement. Dropping that made
    /// the second `cd` invisible, and `..` was then judged one directory too
    /// shallow — an escape, because the word still resolved to somewhere the
    /// boundary accepts.
    #[test]
    fn a_cd_chain_tracks_every_link() {
        let working = tempfile::tempdir().expect("create working dir");
        std::fs::create_dir(working.path().join("sub")).expect("create sub");

        // `cd sub; cd ..` leaves the shell in `working`, so `../../etc/passwd`
        // is `/etc/passwd`. Tracking only the first `cd` judges it from
        // `working/sub`, one level shallower, where it lands in the temp root
        // and is accepted.
        assert_eq!(
            refused("cd sub; cd ..; cat ../../etc/passwd", working.path()),
            [existing("/etc/passwd")]
        );
    }

    /// A newline separates statements exactly as `;` does, but [`line::split`]
    /// folds it into the whitespace between words — so a `cd` at the start of a
    /// later line used to be invisible, and the word after it was judged from
    /// the directory the command started in.
    #[test]
    fn a_cd_on_a_later_line_moves_the_directory() {
        let working = tempfile::tempdir().expect("create working dir");
        let paths = named("cmd\ncd b/c\ncat ../../x", working.path());
        assert_eq!(paths.len(), 2, "{paths:?}");
        assert!(
            Path::new(&paths[1]).starts_with(normalize(working.path())),
            "two levels up from b/c is the working dir; judged from the \
             directory the command started in it lands outside: {paths:?}"
        );
    }

    /// A keyword puts the next word back at command position, so a `cd` inside
    /// an `if` is still a `cd`.
    #[test]
    fn a_cd_after_a_keyword_is_tracked() {
        let working = tempfile::tempdir().expect("create working dir");
        let paths = named("if cd b/c; then cat ../../x; fi", working.path());
        assert_eq!(paths.len(), 2, "{paths:?}");
        assert!(
            Path::new(&paths[1]).starts_with(normalize(working.path())),
            "`../../x` from b/c is inside the working dir: {paths:?}"
        );
    }

    /// A keyword opens a command position only where a command could start.
    /// `echo if cd sub` prints three words; reading the `cd` there as a `cd`
    /// moves the tracked directory into `sub`, and a `..` that really walks out
    /// of the working dir then folds back inside it — the one direction a
    /// wrong guess fails *open*.
    #[test]
    fn a_keyword_that_is_an_argument_does_not_move_the_directory() {
        let working = tempfile::tempdir().expect("create working dir");
        std::fs::create_dir(working.path().join("sub")).expect("create sub");

        // Real bash runs `echo` and then `cat ../../etc/passwd` from `working`;
        // two levels up from a temp dir is `/`, so the file is `/etc/passwd`.
        // Tracked from `working/sub` instead, the same word lands in
        // `/tmp/etc/passwd` — a path that does not exist, so the ordinary-word
        // gate drops it and the command runs.
        assert_eq!(
            refused("echo if cd sub; cat ../../etc/passwd", working.path()),
            [existing("/etc/passwd")]
        );
    }

    /// A file name can hold `=`. Splitting on every one of them checked
    /// `/tmp/a` — a path the command never touches — and never the path it does.
    #[test]
    fn an_equals_inside_a_path_does_not_split_it() {
        let working = tempfile::tempdir().expect("create working dir");
        assert_eq!(
            named("cat /tmp/a=b", working.path()),
            [normalize(Path::new("/tmp/a=b")).display().to_string()]
        );
        // The flag form still separates: that is what the split is for.
        assert_eq!(
            named("cmd --conf=/etc/passwd", working.path()),
            [existing("/etc/passwd")]
        );
    }

    /// `cd 2> err` redirects stderr and goes to `$HOME` — there is no directory
    /// word. Reading `2>` as the target put the word after it inside a
    /// directory the command never enters.
    #[test]
    fn a_redirect_after_cd_is_not_a_directory() {
        let working = tempfile::tempdir().expect("create working dir");
        assert_eq!(
            named("cd 2> err; cat etc/passwd", working.path()),
            Vec::<String>::new(),
            "the directory is unknown, so the relative word is not guessed at"
        );
    }

    #[test]
    fn git_ignore_env_creates_excludes_file_in_data_dir() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let env = git_ignore_env_in(dir.path());

        assert_eq!(env.len(), 3);
        assert_eq!(env[0], ("GIT_CONFIG_COUNT", "1".to_string()));
        assert_eq!(
            env[1],
            ("GIT_CONFIG_KEY_0", "core.excludesfile".to_string())
        );

        let excludes = Path::new(&env[2].1);
        assert!(excludes.starts_with(dir.path()));
        assert_eq!(
            std::fs::read_to_string(excludes).expect("read excludes file"),
            ".jyc/\n"
        );

        // Idempotent: repeated calls neither error nor rewrite
        let env2 = git_ignore_env_in(dir.path());
        assert_eq!(env2, env);
    }

    /// The injected env genuinely makes `git` ignore `.jyc/` in a fresh
    /// repo. Skipped when the `git` binary is unavailable (minimal CI
    /// images, etc.) — the exclude-file mechanics are still covered by
    /// [`git_ignore_env_creates_excludes_file_in_data_dir`].
    #[test]
    fn injected_env_makes_git_ignore_jyc() {
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let dir = tempfile::tempdir().expect("create temp dir");
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(repo.join(".jyc")).expect("create .jyc dir");
        std::fs::write(repo.join(".jyc/secret"), "x").expect("write secret");

        let init = std::process::Command::new("git")
            .arg("init")
            .arg("-q")
            .current_dir(&repo)
            .output()
            .expect("git init");
        assert!(init.status.success(), "git init failed");

        let env = git_ignore_env_in(dir.path());
        let out = std::process::Command::new("git")
            .arg("check-ignore")
            .arg(".jyc/secret")
            .current_dir(&repo)
            .env("GIT_CONFIG_COUNT", env[0].1.as_str())
            .env("GIT_CONFIG_KEY_0", env[1].1.as_str())
            .env("GIT_CONFIG_VALUE_0", env[2].1.as_str())
            .output()
            .expect("git check-ignore");
        assert!(
            out.status.success(),
            ".jyc/secret should be ignored: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
