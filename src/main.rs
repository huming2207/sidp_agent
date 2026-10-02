//! Command-line USB SIDP to local GDB bridge and Soul Injector job manager.
//! See `sidp-agent --help`.
mod commands;

use clap::{Parser, Subcommand};
use commands::{Connection, JobSource};
use sidp_agent::{manage, session};
use std::{
    net::{SocketAddr, TcpListener},
    path::PathBuf,
    process::ExitCode,
};

#[derive(Parser)]
#[command(
    version,
    about = "Soul Injector USB SIDP to local GDB bridge and job manager"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// List serial ports visible to the operating system.
    Ports,
    /// Attach over USB CDC and serve one local GDB connection.
    Gdb {
        #[command(flatten)]
        connection: Connection,
        /// Local GDB address. Only loopback addresses are accepted.
        #[arg(long, default_value = "127.0.0.1:3333")]
        listen: SocketAddr,
        /// Resume on clean GDB detach (default keeps the target halted).
        #[arg(long)]
        resume_on_detach: bool,
    },
    /// Compile target.yaml, optional procedure YAML and images into a job file.
    Compile {
        #[command(flatten)]
        source: JobSource,
        /// Output job file.
        #[arg(short, long, default_value = "job.pb")]
        output: PathBuf,
    },
    /// Query the device.
    Device {
        #[command(subcommand)]
        command: DeviceCommand,
    },
    /// Upload files to the device.
    Asset {
        #[command(subcommand)]
        command: AssetCommand,
    },
    /// Manage the device's programming job.
    Job {
        #[command(subcommand)]
        command: JobCommand,
    },
    /// Collect the device's production log.
    Log {
        #[command(subcommand)]
        command: LogCommand,
    },
}

#[derive(Subcommand)]
enum LogCommand {
    /// Append new log entries to a JSON Lines file, then let the device reuse their space.
    Pull {
        #[command(flatten)]
        connection: Connection,
        /// File to append to; one JSON object per line.
        #[arg(short, long)]
        output: PathBuf,
        /// Leave the entries uncollected on the device (they are pulled again next time).
        #[arg(long)]
        no_ack: bool,
    },
}

#[derive(Subcommand)]
enum DeviceCommand {
    /// Serial number, firmware version and storage use.
    Info {
        #[command(flatten)]
        connection: Connection,
    },
}

#[derive(Subcommand)]
enum AssetCommand {
    /// Upload one file to /data on the device.
    Push {
        #[command(flatten)]
        connection: Connection,
        /// File to upload.
        file: PathBuf,
        /// Device file name; defaults to the local file name.
        #[arg(long)]
        name: Option<String>,
    },
}

#[derive(Subcommand)]
enum JobCommand {
    /// Compile a job, upload it with its images and make it the device's job.
    Push {
        #[command(flatten)]
        connection: Connection,
        #[command(flatten)]
        source: JobSource,
        /// Also run the job whenever a target is plugged in.
        #[arg(long)]
        auto: bool,
    },
    /// Run the device's job once.
    Run {
        #[command(flatten)]
        connection: Connection,
        /// Wait for the result; exits non-zero unless the run passed.
        #[arg(long)]
        wait: bool,
    },
    /// Show the device's job, trigger and last result.
    Status {
        #[command(flatten)]
        connection: Connection,
    },
    /// Stop the run in progress.
    Cancel {
        #[command(flatten)]
        connection: Connection,
        /// Run to cancel; 0 cancels whatever is running.
        #[arg(long, default_value_t = 0)]
        run_id: u32,
    },
}

fn main() -> ExitCode {
    let Err(error) = run(Cli::parse().command) else {
        return ExitCode::SUCCESS;
    };
    match error.downcast_ref::<session::Error>() {
        Some(session::Error::Status(status)) => {
            eprintln!(
                "error: {} (status {status})",
                manage::describe_status(*status)
            );
        }
        _ => eprintln!("error: {error}"),
    }
    ExitCode::FAILURE
}

fn run(command: Command) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        Command::Ports => {
            for port in serialport::available_ports()? {
                println!("{} {:?}", port.port_name, port.port_type);
            }
            Ok(())
        }
        Command::Gdb {
            connection,
            listen,
            resume_on_detach,
        } => serve_gdb(&connection, listen, resume_on_detach),
        Command::Compile { source, output } => commands::compile_job(&source, &output),
        Command::Device {
            command: DeviceCommand::Info { connection },
        } => commands::device_info(&connection),
        Command::Asset {
            command:
                AssetCommand::Push {
                    connection,
                    file,
                    name,
                },
        } => commands::asset_push(&connection, &file, name.as_deref()),
        Command::Job { command } => run_job_command(command),
        Command::Log {
            command:
                LogCommand::Pull {
                    connection,
                    output,
                    no_ack,
                },
        } => commands::log_pull(&connection, &output, no_ack),
    }
}

fn run_job_command(command: JobCommand) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        JobCommand::Push {
            connection,
            source,
            auto,
        } => commands::job_push(&connection, &source, auto),
        JobCommand::Run { connection, wait } => commands::job_run(&connection, wait),
        JobCommand::Status { connection } => commands::job_status(&connection),
        JobCommand::Cancel { connection, run_id } => commands::job_cancel(&connection, run_id),
    }
}

fn serve_gdb(
    connection: &Connection,
    listen: SocketAddr,
    resume_on_detach: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if !listen.ip().is_loopback() {
        return Err("--listen must be a loopback address".into());
    }
    // Reserve the listener before touching the target; bind failure must
    // not leave an otherwise untouched device halted.
    let listener = TcpListener::bind(listen)?;
    let mut session = connection.open()?;
    session.attach()?;
    let info = session.info().unwrap();
    eprintln!(
        "Attached profile={} capabilities=0x{:x}",
        info.profile, info.capabilities
    );
    println!("GDB listening on {}", listener.local_addr()?);
    let requests = sidp_agent::gdb::serve(listener, session, resume_on_detach)?;
    eprintln!("Detached; {requests} SIDP requests sent");
    Ok(())
}
