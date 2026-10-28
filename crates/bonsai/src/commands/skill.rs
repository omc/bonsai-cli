//! `bonsai skill`: a Claude Code skill that tells an agent how to drive this CLI.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::json;

use super::Ctx;
use crate::cli::{SkillCommand, SkillInstallArgs};

const SKILL: &str = include_str!("../../skill/SKILL.md");

pub fn run(ctx: &Ctx, command: SkillCommand) -> Result<()> {
    match command {
        SkillCommand::Install(args) => install(ctx, args),
        SkillCommand::Print => ctx.out.data_bytes(SKILL.as_bytes()),
    }
}

fn install(ctx: &Ctx, args: SkillInstallArgs) -> Result<()> {
    let root = if args.project {
        PathBuf::from(".claude")
    } else {
        claude_dir()?
    };
    let path = root.join("skills").join("bonsai").join("SKILL.md");
    let status = write(&path)?;
    if ctx.out.is_json() {
        return ctx
            .out
            .json(&json!({ "path": path, "status": status.as_str() }));
    }
    let shown = path.display();
    match status {
        Status::Installed => ctx
            .out
            .success(&format!("Installed the bonsai skill in {shown}")),
        Status::Updated => ctx
            .out
            .success(&format!("Updated the bonsai skill in {shown}")),
        Status::Unchanged => ctx
            .out
            .success(&format!("The bonsai skill in {shown} is up to date")),
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Installed,
    Updated,
    Unchanged,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Self::Installed => "installed",
            Self::Updated => "updated",
            Self::Unchanged => "unchanged",
        }
    }
}

/// `$CLAUDE_CONFIG_DIR` if set to an absolute path, as Claude Code reads it, else
/// `~/.claude`.
fn claude_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .filter(|d| d.is_absolute())
    {
        return Ok(dir);
    }
    std::env::home_dir()
        .map(|home| home.join(".claude"))
        .context("couldn't find the home directory; use --project")
}

/// Writes the skill to `path`, creating its directories. A file this version wrote
/// is left alone; any other is replaced, so an upgrade brings the skill up to date.
fn write(path: &Path) -> Result<Status> {
    let status = match std::fs::read_to_string(path) {
        Ok(old) if old == SKILL => return Ok(Status::Unchanged),
        Ok(_) => Status::Updated,
        Err(e) if e.kind() == ErrorKind::NotFound => Status::Installed,
        Err(e) => return Err(e).with_context(|| format!("couldn't read {}", path.display())),
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("couldn't create {}", dir.display()))?;
    }
    std::fs::write(path, SKILL).with_context(|| format!("couldn't write {}", path.display()))?;
    Ok(status)
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;
    use crate::cli::Cli;

    #[test]
    fn writes_then_leaves_alone_then_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("skills/bonsai/SKILL.md");
        assert_eq!(write(&path).unwrap(), Status::Installed);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), SKILL);
        assert_eq!(write(&path).unwrap(), Status::Unchanged);
        std::fs::write(&path, "an older skill").unwrap();
        assert_eq!(write(&path).unwrap(), Status::Updated);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), SKILL);
    }

    #[test]
    fn has_the_frontmatter_claude_code_reads() {
        let front = SKILL
            .strip_prefix("---\n")
            .and_then(|s| s.split_once("\n---\n"))
            .map(|(front, _)| front)
            .expect("SKILL.md starts with --- frontmatter");
        assert!(front.lines().any(|l| l == "name: bonsai"), "{front}");
        assert!(
            front.lines().any(|l| l.starts_with("description: ")),
            "{front}"
        );
    }

    /// Every `bonsai …` span in the skill names a real command, and every `--flag`
    /// in it is one that command takes, so the skill can't drift from the CLI.
    #[test]
    fn every_command_and_flag_in_the_skill_exists() {
        let mut root = Cli::command();
        root.build();
        // Inline spans, and whole lines inside code blocks.
        let mut spans = Vec::new();
        let mut fenced = false;
        for line in SKILL.lines() {
            if line.starts_with("```") {
                fenced = !fenced;
            } else if fenced {
                spans.push(line);
            } else {
                spans.extend(line.split('`').skip(1).step_by(2));
            }
        }
        let spans = spans.into_iter().filter_map(|s| s.strip_prefix("bonsai "));
        let mut checked = 0;
        for span in spans {
            let mut cmd = &root;
            let mut words = span.split_whitespace().peekable();
            while let Some(sub) = words.peek().and_then(|w| cmd.find_subcommand(w)) {
                cmd = sub;
                words.next();
            }
            // `<command>` is a placeholder for any of them.
            if cmd.get_name() == root.get_name() && span.starts_with('<') {
                continue;
            }
            assert!(
                cmd.get_name() != root.get_name(),
                "`bonsai {span}` names no command"
            );
            if cmd.has_subcommands() {
                let next = words.peek().copied().unwrap_or("");
                assert!(
                    next.starts_with('-'),
                    "`bonsai {span}`: {next:?} isn't a subcommand of {}",
                    cmd.get_name()
                );
            }
            for flag in words.filter_map(|w| w.strip_prefix("--")) {
                assert!(
                    flag == "help" || cmd.get_arguments().any(|a| a.get_long() == Some(flag)),
                    "`bonsai {span}`: {} has no --{flag}",
                    cmd.get_name()
                );
            }
            checked += 1;
        }
        assert!(checked > 10, "found only {checked} commands in the skill");
    }
}
