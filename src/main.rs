//! Command-line USB SIDP to local GDB bridge and Soul Injector job manager.
//! See `sidp-agent --help`.
mod commands;
mod network_commands;
mod power_commands;

use clap::{Args, Parser, Subcommand, ValueEnum};
use commands::{Connection, DeviceSession, JobSource};
use sidp_agent::{
    listener::{Event, Listener},
    manage, power,
    session::{self, ResetKind},
};
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
/// Options of the commands that serve GDB.
#[derive(Args)]
struct GdbOptions {
    /// Local GDB address. Only loopback addresses are accepted.
    #[arg(long, default_value = "127.0.0.1:3333")]
    listen: SocketAddr,
    /// Resume on clean GDB detach (default keeps the target halted).
    #[arg(long)]
    resume_on_detach: bool,
    /// Hold the target in reset while connecting, for firmware that sleeps or reuses the SWD pins.
    #[arg(long)]
    connect_under_reset: bool,
    /// Cache flash reads until the target runs again: on by default over
    /// WiFi, off over USB. Turn it off when another core or bus master may
    /// write flash while this core is halted. Also `monitor flash-cache on|off`.
    #[arg(long, value_enum)]
    flash_cache: Option<Toggle>,
}

/// On or off.
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Toggle {
    On,
    Off,
}
#[derive(Subcommand)]
enum Command {
    /// List serial ports visible to the operating system.
    Ports,
    /// Attach over USB CDC and serve one local GDB connection.
    Gdb {
        #[command(flatten)]
        connection: Connection,
        #[command(flatten)]
        options: GdbOptions,
    },
    /// Program the target with a job, halt it at reset and serve one local GDB connection.
    Debug {
        #[command(flatten)]
        connection: Connection,
        #[command(flatten)]
        source: JobSource,
        #[command(flatten)]
        options: GdbOptions,
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
    /// Measure the target's current and supply voltage.
    Power {
        #[command(subcommand)]
        command: PowerCommand,
    },
    /// Hold Soul Injectors that connect over WiFi, for other commands to use
    /// with --port ws://127.0.0.1/DEVICE_ID. Runs until stopped.
    Serve {
        /// Address and port to listen on; the port must match the devices'
        /// server URL.
        #[arg(long, default_value = "0.0.0.0:80")]
        listen: SocketAddr,
    },
    /// Set up how the device connects over WiFi.
    Network {
        #[command(subcommand)]
        command: NetworkCommand,
    },
}

#[derive(Subcommand)]
enum NetworkCommand {
    /// Show the stored network settings and the connection state.
    Show {
        #[command(flatten)]
        connection: Connection,
    },
    /// Store WiFi and server settings over USB; the device uses them after a restart.
    Set {
        #[command(flatten)]
        connection: Connection,
        /// WiFi network name.
        #[arg(long)]
        ssid: String,
        /// Where the device connects to: this computer as the device sees it,
        /// e.g. ws://my-laptop.local/ (port 80) or ws://192.168.1.20:8080/.
        /// Defaults to this computer's mDNS name, ws://HOSTNAME.local/.
        #[arg(long)]
        server: Option<String>,
        /// WiFi password; asked for when omitted (empty for an open network).
        #[arg(long, conflicts_with = "keep_wifi_password")]
        wifi_password: Option<String>,
        /// Keep the WiFi password already stored on the device.
        #[arg(long)]
        keep_wifi_password: bool,
        /// Device password, needed to use the device over the network; asked
        /// for when omitted. Only a salt and verifier are stored on the device.
        #[arg(long, conflicts_with = "keep_password")]
        new_password: Option<String>,
        /// Keep the device password already stored.
        #[arg(long)]
        keep_password: bool,
    },
    /// Turn networking off over USB; the stored settings are kept.
    Off {
        #[command(flatten)]
        connection: Connection,
    },
}

/// A current range of the power profiler.
#[derive(Clone, Copy, ValueEnum)]
enum RangeArg {
    Low,
    Mid,
    High,
}

impl From<RangeArg> for power::Range {
    fn from(range: RangeArg) -> Self {
        match range {
            RangeArg::Low => power::Range::Low,
            RangeArg::Mid => power::Range::Mid,
            RangeArg::High => power::Range::High,
        }
    }
}

#[derive(Subcommand)]
enum PowerCommand {
    /// Show the profiler's ranges and accepted sample settings.
    Info {
        #[command(flatten)]
        connection: Connection,
    },
    /// Record current and voltage to a CSV file.
    Record {
        #[command(flatten)]
        connection: Connection,
        /// CSV file to write: time_s,current_a,vin_v,vout_v,range,settling,gap.
        #[arg(short, long)]
        output: PathBuf,
        /// How long to record, in seconds.
        #[arg(long, default_value_t = 10.0)]
        seconds: f64,
        /// INA226 conversion time in microseconds; see `power info`.
        #[arg(long, default_value_t = 1100)]
        conversion_us: u32,
        /// Conversions averaged into each sample.
        #[arg(long, default_value_t = 1)]
        averages: u32,
        /// Never step below this range.
        #[arg(long, value_enum, default_value_t = RangeArg::Low)]
        lowest_range: RangeArg,
        /// Zero correction written by `power zero`.
        #[arg(long)]
        zero: Option<PathBuf>,
    },
    /// Measure the zero correction with nothing connected to VPP_OUTPUT.
    Zero {
        #[command(flatten)]
        connection: Connection,
        /// JSON file to write, for `power record --zero`.
        #[arg(short, long)]
        output: PathBuf,
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
            options,
        } => attach_and_serve(&connection, &options),
        Command::Debug {
            connection,
            source,
            options,
        } => program_and_serve(&connection, &source, &options),
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
        Command::Power { command } => run_power_command(command),
        Command::Serve { listen } => {
            let listener = Listener::start(listen, |event| match event {
                Event::Connected { id, peer } => println!("{id} connected from {peer}"),
                Event::Disconnected { id } => println!("{id} disconnected"),
            })?;
            let port = listener.local_addr().port();
            println!(
                "Listening on {}. Use a device with --port ws://127.0.0.1:{port}/DEVICE_ID",
                listener.local_addr()
            );
            loop {
                std::thread::park();
            }
        }
        Command::Network { command } => match command {
            NetworkCommand::Show { connection } => network_commands::show(&connection),
            NetworkCommand::Set {
                connection,
                ssid,
                server,
                wifi_password,
                keep_wifi_password,
                new_password,
                keep_password,
            } => network_commands::set(
                &connection,
                &ssid,
                server,
                network_commands::Secret::new(wifi_password, keep_wifi_password),
                network_commands::Secret::new(new_password, keep_password),
            ),
            NetworkCommand::Off { connection } => network_commands::off(&connection),
        },
    }
}

fn run_power_command(command: PowerCommand) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        PowerCommand::Info { connection } => power_commands::info(&connection),
        PowerCommand::Record {
            connection,
            output,
            seconds,
            conversion_us,
            averages,
            lowest_range,
            zero,
        } => {
            let request = power::StartRequest {
                conversion_us,
                averages,
                lowest_range: power::Range::from(lowest_range) as i32,
                duration_ms: 0,
            };
            power_commands::record(&connection, request, seconds, &output, zero.as_deref())
        }
        PowerCommand::Zero { connection, output } => power_commands::zero(&connection, &output),
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

/// Reserves the listener before the target is touched; a bind failure must
/// not leave an otherwise untouched device halted.
fn bind_gdb(options: &GdbOptions) -> Result<TcpListener, Box<dyn std::error::Error>> {
    if !options.listen.ip().is_loopback() {
        return Err("--listen must be a loopback address".into());
    }
    Ok(TcpListener::bind(options.listen)?)
}

fn attach_and_serve(
    connection: &Connection,
    options: &GdbOptions,
) -> Result<(), Box<dyn std::error::Error>> {
    let listener = bind_gdb(options)?;
    let mut session = connection.open()?;
    attach(&mut session, options)?;
    serve_gdb(listener, session, options)
}

fn program_and_serve(
    connection: &Connection,
    source: &JobSource,
    options: &GdbOptions,
) -> Result<(), Box<dyn std::error::Error>> {
    let listener = bind_gdb(options)?;
    let mut session = commands::program_target(connection, source)?;
    attach(&mut session, options)?;
    // The new firmware has been running since programming finished.
    if !options.connect_under_reset {
        session.reset_halt(ResetKind::System)?;
    }
    serve_gdb(listener, session, options)
}

fn attach(session: &mut DeviceSession, options: &GdbOptions) -> session::Result<()> {
    if let Some(toggle) = options.flash_cache {
        session.set_flash_cache(toggle == Toggle::On);
    }
    if options.connect_under_reset {
        session.attach_under_reset()
    } else {
        session.attach()
    }
}

fn serve_gdb(
    listener: TcpListener,
    session: DeviceSession,
    options: &GdbOptions,
) -> Result<(), Box<dyn std::error::Error>> {
    let info = session.info().unwrap();
    eprintln!(
        "Attached profile={} capabilities=0x{:x} flash cache={}",
        info.profile,
        info.capabilities,
        if session.flash_cache() { "on" } else { "off" }
    );
    println!("GDB listening on {}", listener.local_addr()?);
    let served = sidp_agent::gdb::serve(listener, session, options.resume_on_detach)?;
    eprintln!(
        "Detached; target {}; {} SIDP requests sent",
        if served.resumed { "running" } else { "halted" },
        served.requests
    );
    Ok(())
}
