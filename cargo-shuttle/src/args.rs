use std::{
    ffi::OsString,
    fs::create_dir_all,
    io::{self, ErrorKind},
    path::PathBuf,
};

use clap::builder::{OsStringValueParser, PossibleValue, TypedValueParser};
use clap::Parser;
use clap_complete::Shell;
use dunce::canonicalize;
use shuttle_common::{models::project::IDLE_MINUTES, project::ProjectName};
use uuid::Uuid;

use crate::init::Framework;

#[derive(Parser)]
#[command(
    version,
    about,
    // Cargo passes in the subcommand name to the invoked executable. Use a
    // hidden, optional positional argument to deal with it.
    arg(clap::Arg::new("dummy") 
        .value_parser([PossibleValue::new("shuttle")])
        .required(false)
        .hide(true))
)]
pub struct Args {
    /// run this command against the api at the supplied url
    /// (allows targeting a custom deployed instance for this command only)
    #[arg(long, env = "SHUTTLE_API")]
    pub api_url: Option<String>,
    #[command(flatten)]
    pub project_args: ProjectArgs,
    #[command(subcommand)]
    pub cmd: Command,
}

// Common args for subcommands that deal with projects.
#[derive(Parser, Debug)]
pub struct ProjectArgs {
    /// Specify the working directory
    #[arg(global = true, long, default_value = ".", value_parser = OsStringValueParser::new().try_map(parse_path))]
    pub working_directory: PathBuf,
    /// Specify the name of the project (overrides crate name)
    #[arg(global = true, long)]
    pub name: Option<ProjectName>,
}

#[derive(Parser)]
pub enum Command {
    /// deploy a shuttle service
    Deploy(DeployArgs),
    /// manage deployments of a shuttle service
    #[command(subcommand)]
    Deployment(DeploymentCommand),
    /// create a new shuttle service
    Init(InitArgs),
    /// generate shell completions
    Generate {
        /// which shell
        #[arg(short, long, env, default_value_t = Shell::Bash)]
        shell: Shell,
        /// output to file or stdout by default
        #[arg(short, long, env)]
        output: Option<PathBuf>,
    },
    /// view the status of a shuttle service
    Status,
    /// view the logs of a deployment in this shuttle service
    Logs {
        /// Deployment ID to get logs for. Defaults to currently running deployment
        id: Option<Uuid>,

        #[arg(short, long)]
        /// Follow log output
        follow: bool,
    },
    /// remove artifacts that were generated by cargo
    Clean,
    /// stop this shuttle service
    Stop,
    /// manage secrets for this shuttle service
    Secrets,
    /// login to the shuttle platform
    Login(LoginArgs),
    /// log out of the shuttle platform
    Logout,
    /// run a shuttle service locally
    Run(RunArgs),
    /// Open an issue on github and provide feedback.
    Feedback,
    /// manage a project on shuttle
    #[command(subcommand)]
    Project(ProjectCommand),
}

#[derive(Parser)]
pub enum DeploymentCommand {
    /// list all the deployments for a service
    List,
    /// view status of a deployment
    Status {
        /// ID of deployment to get status for
        id: Uuid,
    },
}

#[derive(Parser)]
pub enum ProjectCommand {
    /// create an environment for this project on shuttle
    New {
        #[arg(long, default_value_t = IDLE_MINUTES)]
        /// How long to wait before putting the project in an idle state due to inactivity. 0 means the project will never idle
        idle_minutes: u64,
    },
    /// list all projects belonging to the calling account
    List {
        #[arg(long)]
        /// Return projects filtered by a given project status
        filter: Option<String>,
    },
    /// remove this project environment from shuttle
    Rm,
    /// show the status of this project's environment on shuttle
    Status {
        #[arg(short, long)]
        /// Follow status of project command
        follow: bool,
    },
}

#[derive(Parser, Clone, Debug)]
pub struct LoginArgs {
    /// api key for the shuttle platform
    #[arg(long)]
    pub api_key: Option<String>,
}

#[derive(Parser)]
pub struct DeployArgs {
    /// allow dirty working directories to be packaged
    #[arg(long)]
    pub allow_dirty: bool,
    /// allows pre-deploy tests to be skipped
    #[arg(long)]
    pub no_test: bool,
}

#[derive(Parser, Debug)]
pub struct RunArgs {
    /// port to start service on
    #[arg(long, env, default_value = "8000")]
    pub port: u16,
    /// use 0.0.0.0 instead of localhost (for usage with local external devices)
    #[arg(long)]
    pub external: bool,
    /// Use release mode for building the project.
    #[arg(long, short = 'r')]
    pub release: bool,
}

#[derive(Parser, Debug)]
pub struct InitArgs {
    /// Initialize with actix-web framework
    #[arg(long="actix_web", conflicts_with_all = &["axum", "rocket", "tide", "tower", "poem", "serenity", "poise", "warp", "salvo", "thruster", "no_framework"])]
    pub actix_web: bool,
    /// Initialize with axum framework
    #[arg(long, conflicts_with_all = &["actix_web","rocket", "tide", "tower", "poem", "serenity", "poise", "warp", "salvo", "thruster", "no_framework"])]
    pub axum: bool,
    /// Initialize with rocket framework
    #[arg(long, conflicts_with_all = &["actix_web","axum", "tide", "tower", "poem", "serenity", "poise", "warp", "salvo", "thruster", "no_framework"])]
    pub rocket: bool,
    /// Initialize with tide framework
    #[arg(long, conflicts_with_all = &["actix_web","axum", "rocket", "tower", "poem", "serenity", "poise", "warp", "salvo", "thruster", "no_framework"])]
    pub tide: bool,
    /// Initialize with tower framework
    #[arg(long, conflicts_with_all = &["actix_web","axum", "rocket", "tide", "poem", "serenity", "poise", "warp", "salvo", "thruster", "no_framework"])]
    pub tower: bool,
    /// Initialize with poem framework
    #[arg(long, conflicts_with_all = &["actix_web","axum", "rocket", "tide", "tower", "serenity", "poise", "warp", "salvo", "thruster", "no_framework"])]
    pub poem: bool,
    /// Initialize with salvo framework
    #[arg(long, conflicts_with_all = &["actix_web","axum", "rocket", "tide", "tower", "poem", "warp", "serenity", "poise", "thruster", "no_framework"])]
    pub salvo: bool,
    /// Initialize with serenity framework
    #[arg(long, conflicts_with_all = &["actix_web","axum", "rocket", "tide", "tower", "poem", "warp", "poise", "salvo", "thruster", "no_framework"])]
    pub serenity: bool,
    /// Initialize with poise framework
    #[clap(long, conflicts_with_all = &["actix_web","axum", "rocket", "tide", "tower", "poem", "warp", "serenity", "salvo", "thruster", "no_framework"])]
    pub poise: bool,
    /// Initialize with warp framework
    #[arg(long, conflicts_with_all = &["actix_web","axum", "rocket", "tide", "tower", "poem", "serenity", "poise", "salvo", "thruster", "no_framework"])]
    pub warp: bool,
    /// Initialize with thruster framework
    #[arg(long, conflicts_with_all = &["actix_web","axum", "rocket", "tide", "tower", "poem", "warp", "salvo", "serenity", "poise", "no_framework"])]
    pub thruster: bool,
    /// Initialize without a framework
    #[arg(long, conflicts_with_all = &["actix_web","axum", "rocket", "tide", "tower", "poem", "warp", "salvo", "serenity", "poise", "thruster"])]
    pub no_framework: bool,
    /// Whether to create the environment for this project on Shuttle
    #[arg(long)]
    pub new: bool,
    #[command(flatten)]
    pub login_args: LoginArgs,
    /// Path to initialize a new shuttle project
    #[arg(default_value = ".", value_parser = OsStringValueParser::new().try_map(parse_path) )]
    pub path: PathBuf,
}

impl InitArgs {
    pub fn framework(&self) -> Option<Framework> {
        if self.actix_web {
            Some(Framework::ActixWeb)
        } else if self.axum {
            Some(Framework::Axum)
        } else if self.rocket {
            Some(Framework::Rocket)
        } else if self.tide {
            Some(Framework::Tide)
        } else if self.tower {
            Some(Framework::Tower)
        } else if self.poem {
            Some(Framework::Poem)
        } else if self.salvo {
            Some(Framework::Salvo)
        } else if self.poise {
            Some(Framework::Poise)
        } else if self.serenity {
            Some(Framework::Serenity)
        } else if self.warp {
            Some(Framework::Warp)
        } else if self.thruster {
            Some(Framework::Thruster)
        } else if self.no_framework {
            Some(Framework::None)
        } else {
            None
        }
    }
}

// Helper function to parse and return the absolute path
fn parse_path(path: OsString) -> Result<PathBuf, String> {
    canonicalize(&path).map_err(|e| format!("could not turn {path:?} into a real path: {e}"))
}

// Helper function to parse, create if not exists, and return the absolute path
pub(crate) fn parse_init_path(path: OsString) -> Result<PathBuf, io::Error> {
    // Create the directory if does not exist
    create_dir_all(&path)?;

    parse_path(path.clone()).map_err(|e| {
        io::Error::new(
            ErrorKind::InvalidInput,
            format!("could not turn {path:?} into a real path: {e}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use strum::IntoEnumIterator;

    use super::*;

    fn init_args_factory(framework: &str) -> InitArgs {
        let mut init_args = InitArgs {
            actix_web: false,
            axum: false,
            rocket: false,
            tide: false,
            tower: false,
            poem: false,
            salvo: false,
            serenity: false,
            poise: false,
            warp: false,
            thruster: false,
            no_framework: false,
            new: false,
            login_args: LoginArgs { api_key: None },
            path: PathBuf::new(),
        };

        match framework {
            "actix-web" => init_args.actix_web = true,
            "axum" => init_args.axum = true,
            "rocket" => init_args.rocket = true,
            "tide" => init_args.tide = true,
            "tower" => init_args.tower = true,
            "poem" => init_args.poem = true,
            "salvo" => init_args.salvo = true,
            "serenity" => init_args.serenity = true,
            "poise" => init_args.poise = true,
            "warp" => init_args.warp = true,
            "thruster" => init_args.thruster = true,
            "none" => init_args.no_framework = true,
            _ => unreachable!(),
        }

        init_args
    }

    #[test]
    fn test_init_args_framework() {
        for framework in Framework::iter() {
            let args = init_args_factory(&framework.to_string());
            assert_eq!(args.framework(), Some(framework));
        }
    }
}
