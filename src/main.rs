use anyhow::Result;
use clap::{Parser, Subcommand};

use cmdq::{app, config::Config, shell_integration};

#[derive(Subcommand, Debug)]
enum Command {
    /// Inspect cmdq configuration.
    Config {
        /// Print the effective configuration and its sources.
        #[arg(long)]
        print: bool,
    },
}

#[derive(Parser, Debug)]
#[command(name = "cmdq", version, about = "A PTY-hosted command queue.", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Read configuration from this path instead of the platform default.
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<std::path::PathBuf>,

    /// Shell to spawn (defaults to $SHELL or /bin/sh).
    #[arg(long)]
    shell: Option<String>,

    /// Install OSC 133 shell integration into your rc file and exit.
    #[arg(long)]
    install_integration: bool,

    /// Print the OSC 133 shell integration snippet for the given shell and exit.
    #[arg(long, value_name = "SHELL")]
    print_integration: Option<String>,
}

fn main() -> Result<std::process::ExitCode> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn"))
        .target(env_logger::Target::Stderr)
        .init();

    let cli = Cli::parse();

    if let Some(shell) = cli.print_integration.as_deref() {
        let snippet = shell_integration::snippet_for(shell)?;
        print!("{snippet}");
        return Ok(std::process::ExitCode::SUCCESS);
    }

    if cli.install_integration {
        let report = if let Some(shell) = cli.shell.as_deref() {
            shell_integration::install_for_shell(shell)?
        } else {
            shell_integration::install_for_current_shell()?
        };
        println!("{report}");
        return Ok(std::process::ExitCode::SUCCESS);
    }

    let loaded = Config::load(cli.config.as_deref());
    for warning in &loaded.warnings {
        eprintln!("cmdq: {warning}");
    }
    if let Some(Command::Config { print }) = cli.command {
        if print {
            print!("{}", loaded.print_effective());
        } else {
            println!("{}", loaded.path.display());
        }
        return Ok(std::process::ExitCode::SUCCESS);
    }

    app::run_with_config_exit_status(app::AppConfig { shell: cli.shell }, loaded.config)
        .map(|status| std::process::ExitCode::from(status.min(255) as u8))
}
