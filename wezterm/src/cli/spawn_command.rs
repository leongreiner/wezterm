use crate::cli::resolve_relative_cwd;
use clap::{Parser, ValueHint};
use config::keyassignment::SpawnTabDomain;
use config::ConfigHandle;
use mux::pane::PaneId;
use mux::window::WindowId;
use portable_pty::cmdbuilder::CommandBuilder;
use std::ffi::OsString;
use wezterm_client::client::Client;

#[derive(Debug, Parser, Clone)]
pub struct SpawnCommand {
    /// Specify the current pane.
    /// The default is to use the current pane based on the
    /// environment variable WEZTERM_PANE.
    /// The pane is used to determine the current domain
    /// and window.
    #[arg(long)]
    pane_id: Option<PaneId>,

    #[arg(long)]
    domain_name: Option<String>,

    /// Specify the window into which to spawn a tab.
    /// If omitted, the window associated with the current
    /// pane is used.
    /// Cannot be used with `--workspace` or `--new-window`.
    #[arg(long, conflicts_with_all=&["workspace", "new_window"])]
    window_id: Option<WindowId>,

    /// Spawn into a new window, rather than a new tab.
    #[arg(long)]
    new_window: bool,

    /// Specify the current working directory for the initially
    /// spawned program
    #[arg(long, value_parser, value_hint=ValueHint::DirPath, conflicts_with = "url")]
    cwd: Option<OsString>,

    /// When creating a new window, override the default workspace name
    /// with the provided name.  The default name is "default".
    /// Requires `--new-window`.
    #[arg(long, requires = "new_window")]
    workspace: Option<String>,

    /// Hivegrid build: show the web page at URL in the new tab instead of
    /// running a program. Needs the Hivegrid GUI; a mux server, a mux client
    /// domain or an SSH domain refuses it.
    #[arg(long, value_name = "URL", conflicts_with = "prog")]
    url: Option<String>,

    /// With `--url`: the browser profile (cookies, logins). The default is `main`.
    #[arg(long, requires = "url")]
    profile: Option<String>,

    /// With `--url`: an ephemeral profile that is forgotten with the pane.
    #[arg(long, requires = "url")]
    private: bool,

    /// Instead of executing your shell, run PROG.
    /// For example: `wezterm cli spawn -- bash -l` will spawn bash
    /// as if it were a login shell.
    #[arg(value_parser, value_hint=ValueHint::CommandWithArguments, num_args=1..)]
    prog: Vec<OsString>,
}

impl SpawnCommand {
    /// What the new pane runs: the web pane command for `--url`, `PROG` otherwise
    /// (`None` is the default shell).
    fn pane_command(&self) -> Option<CommandBuilder> {
        if let Some(url) = &self.url {
            Some(mux::webpane::spawn_command(
                url,
                self.profile.as_deref(),
                self.private,
            ))
        } else if self.prog.is_empty() {
            None
        } else {
            Some(CommandBuilder::from_argv(self.prog.clone()))
        }
    }

    pub async fn run(self, client: Client, config: &ConfigHandle) -> anyhow::Result<()> {
        let window_id = if self.new_window {
            None
        } else {
            match self.window_id {
                Some(w) => Some(w),
                None => {
                    let pane_id = client.resolve_pane_id(self.pane_id).await?;

                    let panes = client.list_panes().await?;
                    let mut window_id = None;
                    'outer: for tabroot in panes.tabs {
                        let mut cursor = tabroot.into_tree().cursor();

                        loop {
                            if let Some(entry) = cursor.leaf_mut() {
                                if entry.pane_id == pane_id {
                                    window_id.replace(entry.window_id);
                                    break 'outer;
                                }
                            }
                            match cursor.preorder_next() {
                                Ok(c) => cursor = c,
                                Err(_) => break,
                            }
                        }
                    }
                    window_id
                }
            }
        };

        let workspace = self
            .workspace
            .as_deref()
            .unwrap_or(
                config
                    .default_workspace
                    .as_deref()
                    .unwrap_or(mux::DEFAULT_WORKSPACE),
            )
            .to_string();

        let size = config.initial_size(0, None);

        let command = self.pane_command();
        let spawned = client
            .spawn_v2(codec::SpawnV2 {
                domain: self
                    .domain_name
                    .map_or(SpawnTabDomain::DefaultDomain, |name| {
                        SpawnTabDomain::DomainName(name)
                    }),
                window_id,
                command,
                command_dir: resolve_relative_cwd(self.cwd)?,
                size,
                workspace,
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

    fn parse(args: &[&str]) -> Result<SpawnCommand, clap::Error> {
        // The real command line sets `trailing_var_arg` on the subcommand (cli/mod.rs).
        let matches = SpawnCommand::command()
            .trailing_var_arg(true)
            .try_get_matches_from(std::iter::once("spawn").chain(args.iter().copied()))?;
        SpawnCommand::from_arg_matches(&matches)
    }

    #[test]
    fn url_becomes_the_web_pane_command() {
        let cmd = parse(&[
            "--new-window",
            "--url",
            "https://example.com/",
            "--profile",
            "p",
        ])
        .unwrap()
        .pane_command()
        .unwrap();
        let spawn = mux::webpane::parse_spawn(&cmd).unwrap();
        assert_eq!(spawn.url, "https://example.com/");
        assert_eq!(spawn.profile, "p");
        assert!(!spawn.private);
    }

    #[test]
    fn programs_and_the_default_shell_are_unchanged() {
        assert!(parse(&[]).unwrap().pane_command().is_none());
        let cmd = parse(&["--", "fish"]).unwrap().pane_command().unwrap();
        assert!(mux::webpane::parse_spawn(&cmd).is_none());
        assert_eq!(cmd.get_argv()[0], "fish");
    }

    #[test]
    fn url_conflicts_with_program_and_cwd() {
        assert!(parse(&["--url", "https://a.test/", "--", "bash"]).is_err());
        assert!(parse(&["--url", "https://a.test/", "--cwd", "/tmp"]).is_err());
        assert!(parse(&["--private"]).is_err());
    }
}
