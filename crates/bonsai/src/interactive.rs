//! Whether the CLI may prompt.

use std::io::IsTerminal;

/// True unless `--no-input` was passed, stdin isn't a TTY, or this is CI.
pub fn is_interactive(no_input: bool) -> bool {
    !no_input && std::io::stdin().is_terminal() && !is_ci(std::env::var("CI").ok().as_deref())
}

/// `CI` set to anything but empty, `0` or `false` (any case) means CI.
pub fn is_ci(ci: Option<&str>) -> bool {
    ci.map(str::trim)
        .is_some_and(|v| !(v.is_empty() || v == "0" || v.eq_ignore_ascii_case("false")))
}

#[cfg(test)]
mod tests {
    use super::is_ci;

    #[test]
    fn ci_is_any_value_but_empty_0_or_false() {
        for v in ["1", "true", "yes", "github"] {
            assert!(is_ci(Some(v)), "{v}");
        }
        for v in ["", " ", "0", "false", "FALSE"] {
            assert!(!is_ci(Some(v)), "{v:?}");
        }
        assert!(!is_ci(None));
    }
}
