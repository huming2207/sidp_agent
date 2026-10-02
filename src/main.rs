//! Command-line USB SIDP to local GDB bridge. See `sidp-agent --help`.
use clap::{Parser, Subcommand};
use prost::Message;
use sidp_agent::compile::{self, Sources};
use sidp_agent::{session::Session, transport::UsbCdc};
use std::{
    net::{SocketAddr, TcpListener},
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Parser)]
#[command(
    version,
    about = "Soul Injector USB SIDP to local GDB bridge and job compiler"
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
        /// USB CDC path, e.g. /dev/ttyACM0 or COM3.
        #[arg(long)]
        port: String,
        /// Local GDB address. Only loopback addresses are accepted.
        #[arg(long, default_value = "127.0.0.1:3333")]
        listen: SocketAddr,
        /// CDC line coding; does not set SWD speed.
        #[arg(long, default_value_t = 115200)]
        baud: u32,
        /// Per-operation timeout in milliseconds (no automatic retries).
        #[arg(long,default_value_t=5000,value_parser=clap::value_parser!(u64).range(1..=60000))]
        timeout_ms: u64,
        /// Wait after DTR assertion for the firmware to accept the connection.
        #[arg(long,default_value_t=250,value_parser=clap::value_parser!(u64).range(0..=60000))]
        settle_ms: u64,
        /// Resume on clean GDB detach (default keeps the target halted).
        #[arg(long)]
        resume_on_detach: bool,
        /// Skip DTR modem control for PTY testing only; real hardware requires DTR.
        #[arg(long)]
        no_dtr: bool,
    },
    /// Compile target.yaml and optional procedure YAML into a job file.
    Compile {
        /// probe-rs style target description.
        #[arg(long)]
        target: PathBuf,
        /// Variant to select when target.yaml lists several.
        #[arg(long)]
        variant: Option<String>,
        /// Procedure run before target detection (pre_prog.yaml).
        #[arg(long)]
        pre: Option<PathBuf>,
        /// Procedure run after self tests (post_prog.yaml).
        #[arg(long)]
        post: Option<PathBuf>,
        /// Output job file; copy it to the device as /data/job.pb.
        #[arg(short, long, default_value = "job.pb")]
        output: PathBuf,
    },
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    match Cli::parse().command {
        Command::Ports => {
            for port in serialport::available_ports()? {
                println!("{} {:?}", port.port_name, port.port_type);
            }
        }
        Command::Gdb {
            port,
            listen,
            baud,
            timeout_ms,
            settle_ms,
            resume_on_detach,
            no_dtr,
        } => {
            if !listen.ip().is_loopback() {
                return Err("--listen must be a loopback address".into());
            }
            // Reserve the listener before touching the target; bind failure must
            // not leave an otherwise untouched device halted.
            let listener = TcpListener::bind(listen)?;
            let usb = UsbCdc::open(&port, baud, Duration::from_millis(settle_ms), !no_dtr)?;
            let mut session = Session::new(usb, Duration::from_millis(timeout_ms));
            session.attach()?;
            let info = session.info().unwrap();
            eprintln!(
                "Attached profile={} capabilities=0x{:x}",
                info.profile, info.capabilities
            );
            println!("GDB listening on {}", listener.local_addr()?);
            let requests = sidp_agent::gdb::serve(listener, session, resume_on_detach)?;
            eprintln!("Detached; {requests} SIDP requests sent");
        }
        Command::Compile {
            target,
            variant,
            pre,
            post,
            output,
        } => compile_job(
            &target,
            variant.as_deref(),
            pre.as_deref(),
            post.as_deref(),
            &output,
        )?,
    }
    Ok(())
}

fn read_optional(path: Option<&Path>) -> std::io::Result<Option<String>> {
    path.map(std::fs::read_to_string).transpose()
}

fn compile_job(
    target: &Path,
    variant: Option<&str>,
    pre: Option<&Path>,
    post: Option<&Path>,
    output: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let target_text = std::fs::read_to_string(target)?;
    let pre_text = read_optional(pre)?;
    let post_text = read_optional(post)?;
    let sources = Sources {
        target: &target_text,
        variant,
        pre_program: pre_text.as_deref(),
        post_program: post_text.as_deref(),
    };
    let compiled = compile::compile(&sources)?;
    for warning in &compiled.warnings {
        eprintln!("warning: {warning}");
    }
    let bytes = compiled.job.encode_to_vec();
    std::fs::write(output, &bytes)?;
    println!("Wrote {} ({} bytes)", output.display(), bytes.len());
    Ok(())
}
