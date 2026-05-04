use anyhow::Result;
use clap::Parser;

use cmdq::{app, shell_integration};

#[derive(Parser, Debug)]
#[command(name = "cmdq", version, about = "A PTY-hosted command queue.", long_about = None)]
struct Cli {
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

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn"))
        .target(env_logger::Target::Stderr)
        .init();

    let cli = Cli::parse();

    if let Some(shell) = cli.print_integration.as_deref() {
        let snippet = shell_integration::snippet_for(shell)?;
        print!("{snippet}");
        return Ok(());
    }

    if cli.install_integration {
        let report = if let Some(shell) = cli.shell.as_deref() {
            shell_integration::install_for_shell(shell)?
        } else {
            shell_integration::install_for_current_shell()?
        };
        println!("{report}");
        return Ok(());
    }

    app::run(app::AppConfig { shell: cli.shell })
}
