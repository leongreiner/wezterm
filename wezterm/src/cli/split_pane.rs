use crate::cli::resolve_relative_cwd;
use clap::{Parser, ValueHint};
use mux::pane::PaneId;
use mux::tab::{SplitDirection, SplitRequest, SplitSize};
use portable_pty::cmdbuilder::CommandBuilder;
use std::ffi::OsString;
use std::path::PathBuf;
use wezterm_client::client::Client;

#[derive(Debug, Parser, Clone)]
pub struct SplitPane {
    /// Specify the pane that should be split.
    /// The default is to use the current pane based on the
    /// environment variable WEZTERM_PANE.
    #[arg(long)]
    pane_id: Option<PaneId>,

    /// Equivalent to `--right`. If neither this nor any other direction
    /// is specified, the default is equivalent to `--bottom`.
    #[arg(long, conflicts_with_all=&["left", "right", "top", "bottom"])]
    horizontal: bool,

    /// Split horizontally, with the new pane on the left
    #[arg(long, conflicts_with_all=&["right", "top", "bottom"])]
    left: bool,

    /// Split horizontally, with the new pane on the right
    #[arg(long, conflicts_with_all=&["left", "top", "bottom"])]
    right: bool,

    /// Split vertically, with the new pane on the top
    #[arg(long, conflicts_with_all=&["left", "right", "bottom"])]
    top: bool,

    /// Split vertically, with the new pane on the bottom
    #[arg(long, conflicts_with_all=&["left", "right", "top"])]
    bottom: bool,

    /// Rather than splitting the active pane, split the entire
    /// window.
    #[arg(long)]
    top_level: bool,

    /// The number of cells that the new split should have.
    /// If omitted, 50% of the available space is used.
    #[arg(long)]
    cells: Option<usize>,

    /// Specify the number of cells that the new split should
    /// have, expressed as a percentage of the available space.
    #[arg(long, conflicts_with = "cells")]
    percent: Option<u8>,

    /// Specify the current working directory for the initially
    /// spawned program
    #[arg(long, value_parser, value_hint=ValueHint::DirPath, conflicts_with = "url")]
    cwd: Option<OsString>,

    /// Hivegrid build: show the web page at URL in the new pane instead of
    /// running a program. Needs the Hivegrid GUI; a mux server, a mux client
    /// domain or an SSH domain refuses it.
    #[arg(long, value_name = "URL", conflicts_with_all=&["prog", "move_pane_id"])]
    url: Option<String>,

    /// With `--url`: the browser profile (cookies, logins). The default is `main`.
    #[arg(long, requires = "url")]
    profile: Option<String>,

    /// With `--url`: an ephemeral profile that is forgotten with the pane.
    #[arg(long, requires = "url")]
    private: bool,

    /// With `--url`: the Chromium-family browser binary for this pane, over HIVEGRID_CHROME
    /// and the Settings choice (a browser already running for the profile is joined).
    #[arg(long, value_name = "PATH", value_hint=ValueHint::FilePath, requires = "url")]
    browser: Option<PathBuf>,

    /// Instead of spawning a new command, move the specified
    /// pane into the newly created split.
    #[arg(long, conflicts_with_all=&["cwd", "prog", "url"])]
    move_pane_id: Option<PaneId>,

    /// Instead of executing your shell, run PROG.
    /// For example: `wezterm cli split-pane -- bash -l` will spawn bash
    /// as if it were a login shell.
    #[arg(value_parser, value_hint=ValueHint::CommandWithArguments, num_args=1..)]
    prog: Vec<OsString>,
}

impl SplitPane {
    /// What the new pane runs: the web pane command for `--url`, `PROG` otherwise
    /// (`None` is the default shell).
    fn pane_command(&self) -> Option<CommandBuilder> {
        if let Some(url) = &self.url {
            Some(mux::webpane::spawn_command(
                url,
                self.profile.as_deref(),
                self.private,
                self.browser.as_deref(),
            ))
        } else if self.prog.is_empty() {
            None
        } else {
            Some(CommandBuilder::from_argv(self.prog.clone()))
        }
    }

    pub async fn run(self, client: Client) -> anyhow::Result<()> {
        let pane_id = client.resolve_pane_id(self.pane_id).await?;

        let direction = if self.left || self.right || self.horizontal {
            SplitDirection::Horizontal
        } else if self.top || self.bottom {
            SplitDirection::Vertical
        } else {
            SplitDirection::Vertical
        };
        let target_is_second = !(self.left || self.top);
        let size = match (self.cells, self.percent) {
            (Some(c), _) => SplitSize::Cells(c),
            (_, Some(p)) => SplitSize::Percent(p),
            (None, None) => SplitSize::Percent(50),
        };

        let split_request = SplitRequest {
            direction,
            target_is_second,
            size,
            top_level: self.top_level,
        };

        let command = self.pane_command();
        let spawned = client
            .split_pane(codec::SplitPane {
                pane_id,
                split_request,
                domain: config::keyassignment::SpawnTabDomain::CurrentPaneDomain,
                command,
                command_dir: resolve_relative_cwd(self.cwd)?,
                move_pane_id: self.move_pane_id,
            })
            .await?;

        log::debug!("{:?}", spawned);
        println!("{}", spawned.pane_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, FromArgMatches};

    fn parse(args: &[&str]) -> Result<SplitPane, clap::Error> {
        // The real command line sets `trailing_var_arg` on the subcommand (cli/mod.rs).
        let matches = SplitPane::command()
            .trailing_var_arg(true)
            .try_get_matches_from(std::iter::once("split-pane").chain(args.iter().copied()))?;
        SplitPane::from_arg_matches(&matches)
    }

    fn argv(cmd: &CommandBuilder) -> Vec<String> {
        cmd.get_argv()
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn url_becomes_the_web_pane_command() {
        let cmd = parse(&["--right", "--url", "https://example.com/"])
            .unwrap()
            .pane_command()
            .unwrap();
        assert_eq!(argv(&cmd), ["hivegrid:web", "https://example.com/"]);
        let spawn = mux::webpane::parse_spawn(&cmd).unwrap();
        assert_eq!(spawn.profile, "main");
        assert!(!spawn.private);
    }

    #[test]
    fn profile_and_private_travel_in_the_environment() {
        let cmd = parse(&[
            "--url",
            "data:text/html,x",
            "--profile",
            "work",
            "--private",
        ])
        .unwrap()
        .pane_command()
        .unwrap();
        let spawn = mux::webpane::parse_spawn(&cmd).unwrap();
        assert_eq!(spawn.url, "data:text/html,x");
        assert_eq!(spawn.profile, "work");
        assert!(spawn.private);
    }

    #[test]
    fn browser_travels_with_the_command_as_an_absolute_path() {
        let spawn = |args: &[&str]| {
            mux::webpane::parse_spawn(&parse(args).unwrap().pane_command().unwrap()).unwrap()
        };
        assert_eq!(
            spawn(&["--url", "about:blank", "--browser", "/opt/b/chrome"]).browser,
            Some(PathBuf::from("/opt/b/chrome"))
        );
        assert_eq!(spawn(&["--url", "about:blank"]).browser, None);
        // The GUI's working directory is not the caller's.
        let rel = spawn(&["--url", "about:blank", "--browser", "wrap/chrome"])
            .browser
            .unwrap();
        assert!(rel.is_absolute() && rel.ends_with("wrap/chrome"), "{rel:?}");
    }

    #[test]
    fn programs_and_the_default_shell_are_unchanged() {
        assert!(parse(&[]).unwrap().pane_command().is_none());
        let cmd = parse(&["--", "bash", "-l"])
            .unwrap()
            .pane_command()
            .unwrap();
        assert_eq!(argv(&cmd), ["bash", "-l"]);
        assert!(mux::webpane::parse_spawn(&cmd).is_none());
    }

    #[test]
    fn url_conflicts_with_program_cwd_and_move() {
        assert!(parse(&["--url", "https://a.test/", "--", "bash"]).is_err());
        assert!(parse(&["--url", "https://a.test/", "--cwd", "/tmp"]).is_err());
        assert!(parse(&["--url", "https://a.test/", "--move-pane-id", "3"]).is_err());
    }

    #[test]
    fn profile_and_private_need_a_url() {
        assert!(parse(&["--profile", "work"]).is_err());
        assert!(parse(&["--private"]).is_err());
        assert!(parse(&["--browser", "/opt/b/chrome"]).is_err());
    }
}
