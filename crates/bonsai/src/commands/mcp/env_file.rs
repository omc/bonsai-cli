//! `write_cluster_url`'s env file: a path inside the project that git ignores, and
//! one `NAME=value` line in it.

use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use secrecy::{ExposeSecret, SecretString};
use url::Url;

/// A checked env file: below the project folder, not a link, and ignored by git
/// when the folder is in a repository.
#[derive(Debug)]
pub struct Target {
    /// Relative to the project folder.
    rel: PathBuf,
}

/// What was written, without the secret.
#[derive(Debug)]
pub struct Written {
    pub path: String,
    pub redacted: String,
    pub replaced: bool,
}

impl Target {
    pub fn check(root: &Path, path: &str) -> Result<Self> {
        let rel = Path::new(path);
        let plain = !path.is_empty()
            && rel
                .components()
                .all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
        if !plain || rel.file_name().is_none() {
            bail!("{path:?} must be a file path inside the project folder, with no `..`");
        }
        let full = root.join(rel);
        let dir = full.parent().unwrap_or(root);
        let real_root = root
            .canonicalize()
            .with_context(|| root.display().to_string())?;
        let real_dir = dir
            .canonicalize()
            .with_context(|| format!("{path}: its folder doesn't exist"))?;
        if !real_dir.starts_with(&real_root) {
            bail!("{path:?} leads outside the project folder");
        }
        match std::fs::symlink_metadata(&full) {
            Ok(m) if !m.is_file() => bail!("{path:?} isn't a regular file"),
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e).context(path.to_owned()),
        }
        check_ignored(root, path)?;
        Ok(Self {
            rel: rel.to_path_buf(),
        })
    }

    /// Sets `var` to `url` in the file, replacing any line that set it, and leaves the
    /// file with mode 0600.
    pub fn write(&self, root: &Path, var: &str, url: &SecretString) -> Result<Written> {
        let full = root.join(&self.rel);
        let old = match std::fs::read_to_string(&full) {
            Ok(s) => s,
            Err(e) if e.kind() == ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e).context(self.rel.display().to_string()),
        };
        let (text, replaced) = upsert(&old, var, url.expose_secret());
        let dir = full.parent().unwrap_or(root);
        let name = full
            .file_name()
            .and_then(|n| n.to_str())
            .context("the file name isn't UTF-8")?;
        bonsai_core::replace_file(dir, name, &text)
            .with_context(|| self.rel.display().to_string())?;
        Ok(Written {
            path: self.rel.display().to_string(),
            redacted: redact(url.expose_secret()),
            replaced,
        })
    }
}

/// Refuses a file git would commit: tracked, or not ignored. Outside a repository
/// there is nothing to check; without git, a folder in a repository is refused.
fn check_ignored(root: &Path, path: &str) -> Result<()> {
    let git = |args: &[&str]| Command::new("git").arg("-C").arg(root).args(args).output();
    let inside = match git(&["rev-parse", "--is-inside-work-tree"]) {
        Ok(out) => out.status.success() && out.stdout.starts_with(b"true"),
        Err(e) if e.kind() == ErrorKind::NotFound => {
            if root.ancestors().any(|d| d.join(".git").exists()) {
                bail!("git isn't installed, so whether {path:?} is ignored can't be checked");
            }
            return Ok(());
        }
        Err(e) => return Err(e).context("couldn't run git"),
    };
    if !inside {
        return Ok(());
    }
    // Exit 0: ignored. 1: not ignored, or tracked (check-ignore skips tracked files).
    let out = git(&["check-ignore", "-q", "--", path]).context("couldn't run git")?;
    match out.status.code() {
        Some(0) => Ok(()),
        Some(1) => bail!(
            "{path:?} isn't ignored by git, or is tracked; add it to .gitignore \
             (and `git rm --cached` it if tracked) so the password isn't committed"
        ),
        _ => bail!(
            "git check-ignore failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ),
    }
}

/// `text` with `var` set to `value`: the first line that set it (`VAR=` or
/// `export VAR=`) replaced in place, any later ones dropped, or a line appended.
/// Returns whether a line was replaced.
fn upsert(text: &str, var: &str, value: &str) -> (String, bool) {
    let sets = |line: &str| {
        let line = line.trim_start();
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        line.strip_prefix(var)
            .is_some_and(|rest| rest.trim_start().starts_with('='))
    };
    let new = format!("{var}={value}");
    let mut out = Vec::new();
    let mut replaced = false;
    for line in text.lines() {
        if sets(line) {
            if !replaced {
                out.push(new.as_str());
                replaced = true;
            }
        } else {
            out.push(line);
        }
    }
    if !replaced {
        out.push(&new);
    }
    let mut s = out.join("\n");
    s.push('\n');
    (s, replaced)
}

/// `url` with its password as `****`; an unparsable URL is withheld whole.
fn redact(url: &str) -> String {
    match Url::parse(url) {
        Ok(mut u) => {
            if u.password().is_some() {
                let _ = u.set_password(Some("****"));
            }
            u.to_string()
        }
        Err(_) => "<withheld>".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_appends_a_new_variable() {
        assert_eq!(upsert("", "A", "1"), ("A=1\n".into(), false));
        assert_eq!(upsert("X=2", "A", "1"), ("X=2\nA=1\n".into(), false));
    }

    #[test]
    fn upsert_replaces_in_place_and_drops_repeats() {
        let (text, replaced) = upsert("X=1\nexport A = old\nY=2\nA=older\n", "A", "new");
        assert!(replaced);
        assert_eq!(text, "X=1\nA=new\nY=2\n");
    }

    #[test]
    fn upsert_leaves_longer_names_alone() {
        let (text, replaced) = upsert("AB=1\n# A=2\n", "A", "3");
        assert!(!replaced);
        assert_eq!(text, "AB=1\n# A=2\nA=3\n");
    }

    #[test]
    fn redact_hides_only_the_password() {
        assert_eq!(
            redact("https://user:s3cret@host.bonsai.io"),
            "https://user:****@host.bonsai.io/"
        );
        assert_eq!(redact("not a url s3cret"), "<withheld>");
    }

    #[test]
    fn paths_must_stay_inside_the_project() {
        let dir = tempfile::tempdir().unwrap();
        for bad in ["", "..", "../.env", "/etc/x", "a/../../b", "."] {
            assert!(Target::check(dir.path(), bad).is_err(), "{bad:?}");
        }
        assert!(Target::check(dir.path(), "missing-dir/.env").is_err());
        assert!(Target::check(dir.path(), ".env").is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_folder_out_of_the_project_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("out")).unwrap();
        assert!(Target::check(dir.path(), "out/.env").is_err());
        std::os::unix::fs::symlink(outside.path().join("f"), dir.path().join("link")).unwrap();
        assert!(Target::check(dir.path(), "link").is_err());
    }
}
