//! `bonsai`: the Bonsai CLI.

mod cli;
mod commands;
#[cfg(test)]
mod coverage;
mod display;
mod exit;
mod interactive;
mod output;
mod render_cluster;
mod report;
mod sanitize;
mod update;

use std::io::Write;
use std::sync::atomic::{AtomicU8, Ordering};

use clap::{CommandFactory, FromArgMatches};
use tokio_util::sync::CancellationToken;

use crate::cli::{Cli, Command};
use crate::commands::Ctx;
use crate::output::Printer;

fn main() {
    let code = run();
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    // `exit`, not a return: a blocking stdin read abandoned at Ctrl-C would make the
    // runtime's drop wait for it.
    std::process::exit(code.into());
}

fn run() -> u8 {
    let matches = match Cli::command().try_get_matches() {
        Ok(m) => m,
        Err(e) => {
            // Help and --version exit 0, usage errors 2.
            let _ = e.print();
            return u8::try_from(e.exit_code()).unwrap_or(2);
        }
    };
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(cli) => cli,
        Err(e) => {
            let _ = e.print();
            return 2;
        }
    };
    init_tracing(cli.global.verbose);
    std::panic::set_hook(Box::new(|info| {
        eprintln!("{}", crash_line(&info.to_string()));
    }));

    let out = Printer::new(cli.global.json, cli.global.quiet);
    // `bonsai api` always speaks JSON, errors included.
    let json_errors = cli.global.json || matches!(cli.command, Command::Api(_));
    let config_dir = match bonsai_core::paths::config_dir() {
        Ok(dir) => dir,
        Err(e) => {
            out.error(&e.to_string());
            return 2;
        }
    };
    let ctx = Ctx {
        out,
        interactive: interactive::is_interactive(cli.global.no_input),
        global: cli.global,
        cancel: CancellationToken::new(),
        config_dir,
    };

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            ctx.out.error(&format!("couldn't start: {e}"));
            return 1;
        }
    };
    watch_ctrl_c(ctx.cancel.clone());
    // Not for `completion`: a shell profile runs it on every new shell.
    let update = (update::wanted(ctx.global.quiet)
        && !matches!(cli.command, Command::Completion(_)))
    .then(|| runtime.spawn(update::check(ctx.config_dir.clone())));
    let result = runtime.block_on(commands::run(cli.command, &ctx));
    let code = match result {
        Ok(()) => 0,
        Err(e) => {
            let api_url = ctx.settings().ok().map(|s| s.api_url);
            report::report(&ctx.out, json_errors, &e, api_url.as_ref());
            exit::exit_code(&e)
        }
    };
    if let Some(update) = update
        && !ctx.cancel.is_cancelled()
        && let Ok(Ok(Some(line))) = runtime.block_on(tokio::time::timeout(update::WAIT, update))
    {
        ctx.out.info(&line);
    }
    code
}

/// The first Ctrl-C fires `cancel`, so a wait stops and reports what it left; the
/// second exits at once with 130.
///
/// The handler runs on its own OS thread (`ctrlc`), not on the runtime: the
/// current-thread runtime can sit in synchronous code (a keychain call, a file
/// read), and a signal task on it wouldn't run until that returned.
fn watch_ctrl_c(cancel: CancellationToken) {
    static SEEN: AtomicU8 = AtomicU8::new(0);
    let installed = ctrlc::set_handler(move || {
        if SEEN.fetch_add(1, Ordering::SeqCst) == 0 {
            cancel.cancel();
        } else {
            // No explicit flush: the main thread may hold stdout's or stderr's lock
            // for a write blocked on a full pipe, and flushing would wait on it.
            // `exit` flushes stdout itself with a `try_lock`.
            std::process::exit(130);
        }
    });
    if let Err(e) = installed {
        tracing::debug!(error = %e, "can't listen for Ctrl-C");
    }
}

/// By default only `bonsai-core`'s warnings (a loosened `credentials.toml`, a failed
/// cleanup), as `WARN <message>` lines; `-v` logs requests from this CLI's crates,
/// `-vv` everything they trace, with times and targets. Response bodies are never
/// logged.
fn init_tracing(verbose: u8) {
    let filter = match verbose {
        0 => "bonsai_core=warn",
        1 => "bonsai=debug,bonsai_api=debug,bonsai_core=debug",
        _ => "bonsai=trace,bonsai_api=trace,bonsai_core=trace",
    };
    let fmt = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
        .with_writer(output::SanitizedStderr)
        .with_ansi(false);
    if verbose == 0 {
        fmt.without_time().with_target(false).init();
    } else {
        fmt.init();
    }
}

/// The panic hook's line. A panic's text can carry server strings (an `expect` on
/// a header), so it goes through `sanitize` like any other API text.
fn crash_line(info: &str) -> String {
    format!(
        "✗ bonsai crashed, which is a bug: {}",
        sanitize::sanitize(info, false)
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_crash_line_is_sanitized() {
        let line = super::crash_line("bad header \u{1b}[2J\u{202e}x\r");
        assert!(!line.contains(['\u{1b}', '\u{202e}', '\r']), "{line:?}");
        assert!(line.starts_with("✗ bonsai crashed"));
    }
}
