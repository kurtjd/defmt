//! # qemu-run
//!
//! An alternative to the [`probe-run`](https://github.com/knurling-rs/probe-run) printer,
//! used by [`defmt`](https://github.com/knurling-rs/defmt).
//!
//! Parses data sent by QEMU over semihosting (ARM Cortex-M only).

use std::{
    env, fs,
    io::prelude::*,
    process::{self, Command, Stdio},
};

use anyhow::{anyhow, bail, Context};
use clap::{Parser, ValueEnum};
use defmt_decoder::{
    log::{
        format::{Formatter, FormatterConfig, HostFormatter},
        DefmtLoggerType,
    },
    DecodeError, Frame, Locations, StreamDecoder, Table,
};
use process::Child;

/// Run a QEMU system emulator, takes defmt logs from semihosting output and prints them to stdout
#[derive(clap::Parser, Clone)]
#[command(name = "qemu-run")]
struct Opts {
    /// The firmware running on the device being logged
    #[arg(required = true)]
    elf: Option<std::path::PathBuf>,

    /// Specify the target architecture
    #[arg(long, default_value = "arm")]
    arch: Arch,

    /// Specify the QEMU machine type
    #[arg(long, required = true)]
    machine: Option<String>,

    /// Optionally specify the QEMU CPU type
    #[arg(long, required = false)]
    cpu: Option<String>,

    /// Optionally specify a custom defmt log format
    #[arg(short = 'l', required = false, alias = "log-format")]
    log_format: Option<String>,

    /// Print the version number, and quit
    #[arg(short = 'V', long, exclusive = true)]
    version: bool,

    /// Set up UART0 as a telnet server instead of piping to the console
    #[arg(short = 't', long, alias = "uart-telnet", conflicts_with = "uart_pty")]
    uart_telnet: bool,

    /// Redirect UART0 to a PTY (QEMU will print the allocated device path)
    #[arg(long, alias = "uart-pty", conflicts_with = "uart_telnet")]
    uart_pty: bool,

    /// Specify a BIOS file, or "none" to disable the default BIOS
    #[arg(long)]
    bios: Option<String>,

    /// Print verbose log output
    #[arg(short = 'v', long)]
    verbose: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum Arch {
    Arm,
    Aarch64,
    Riscv32,
    Riscv64,
}

impl Arch {
    fn qemu_binary(self) -> &'static str {
        match self {
            Arch::Arm => "qemu-system-arm",
            Arch::Aarch64 => "qemu-system-aarch64",
            Arch::Riscv32 => "qemu-system-riscv32",
            Arch::Riscv64 => "qemu-system-riscv64",
        }
    }
}

fn main() -> Result<(), anyhow::Error> {
    notmain().map(|opt_code| {
        if let Some(code) = opt_code {
            process::exit(code);
        }
    })
}

fn notmain() -> Result<Option<i32>, anyhow::Error> {
    let opts = Opts::parse();

    if opts.version {
        return print_version();
    }

    let Some(elf_path) = opts.elf else {
        bail!("ELF filename is required. Syntax: `qemu-run -machine <machine> <path-to-elf>`.");
    };

    let Some(machine) = opts.machine else {
        bail!("Machine type is required. Syntax: `qemu-run -machine <machine> <path-to-elf>`.");
    };

    if opts.verbose {
        log::info!("QEMU machine is {:?}", machine);
        if let Some(cpu) = &opts.cpu {
            log::info!("QEMU cpu is {:?}", cpu);
        }
    }

    //
    // Process the ELF file
    //

    let bytes = fs::read(&elf_path)
        .with_context(|| format!("Failed to load file {}", elf_path.display()))?;

    let table = if env::var_os("QEMU_RUN_IGNORE_VERSION").is_some() {
        Table::parse_ignore_version(&bytes)
    } else {
        Table::parse(&bytes)
    };
    let table = match table {
        Ok(Some(table)) => table,
        Ok(None) => {
            bail!("Loaded ELF but did not find a .defmt section");
        }
        Err(e) => {
            bail!("Failed to load ELF: {:?}", e);
        }
    };
    // check if the locations info contains all the indicies
    let locs = table.get_locations(&bytes)?;
    let locs = if table.indices().all(|idx| locs.contains_key(&(idx as u64))) {
        Some(locs)
    } else {
        eprintln!("(NOTE) location info is incomplete; it will be omitted from the output");
        None
    };

    //
    // Configure defmt logging
    //
    let (mut formatter_config, host_formatter_config) = match &opts.log_format {
        Some(config) => (
            FormatterConfig::custom(config),
            FormatterConfig::custom(config),
        ),
        None => (FormatterConfig::default(), FormatterConfig::default()),
    };
    formatter_config.is_timestamp_available = table.has_timestamp();
    let formatter = Formatter::new(formatter_config);
    let host_formatter = HostFormatter::new(host_formatter_config);
    defmt_decoder::log::init_logger(formatter, host_formatter, DefmtLoggerType::Stdout, |_| true);

    //
    // Set up the qemu-system command line
    //

    let mut command = Command::new(opts.arch.qemu_binary());
    // set the mandatory machine type
    command.args(["-machine", &machine]);
    // set the optional CPU type
    if let Some(cpu) = &opts.cpu {
        command.arg("-cpu");
        command.arg(cpu);
    }
    // create a telnet server and tell QEMU to use it for UART0. Connect to localhost:4321 to interact with it.
    if opts.uart_telnet {
        command.arg("-chardev");
        command.arg("socket,id=sock0,server=on,telnet=on,port=4321,host=localhost");
        command.args(["-serial", "chardev:sock0"]);
        log::info!(
            "Told QEMU to start telnet server on localhost:4321. Connect to interact with UART0."
        );
    // create a PTY and tell QEMU to use it for UART0. QEMU will print the allocated device path on startup.
    } else if opts.uart_pty {
        command.args(["-serial", "pty"]);
    // create a character device connected to `uart_socket`
    } else {
        let uart_socket = std::net::TcpListener::bind("localhost:0")
            .with_context(|| "Binding free port on localhost")?;
        let uart_socket_addr = uart_socket
            .local_addr()
            .with_context(|| "Getting socket address")?;
        if opts.verbose {
            log::info!("Bound UART data socket to {:?}", uart_socket_addr);
        }
        std::thread::spawn(move || {
            print_loop(uart_socket);
        });
        command.arg("-chardev");
        command.arg(format!(
            "socket,id=sock0,server=off,telnet=off,port={},host=localhost",
            uart_socket_addr.port()
        ));
        command.args(["-serial", "chardev:sock0"]);
    }
    // disable the graphical output
    command.arg("-nographic");
    // disable the command monitor
    command.args(["-monitor", "none"]);
    // send semihosting to stdout
    command.args(["-semihosting-config", "enable=on,target=native"]);
    // set the BIOS (if specified)
    if let Some(bios) = &opts.bios {
        command.args(["-bios", bios]);
    }
    // set the firmware to load
    command.arg("-kernel");
    command.arg(elf_path);
    // grab stdout
    command.stdout(Stdio::piped());

    //
    // Run QEMU
    //

    let mut child = KillOnDrop(command.spawn().unwrap_or_else(|_| {
        panic!(
            "Error running {}; perhaps you haven't installed it yet?",
            opts.arch.qemu_binary()
        )
    }));

    //
    // Decode stdout as defmt data
    //

    let current_dir = std::env::current_dir()?;

    let mut stdout = child
        .0
        .stdout
        .take()
        .ok_or_else(|| anyhow!("failed to acquire child's stdout handle"))?;

    // QEMU prints "char device redirected to ..." on stdout when using -serial pty,
    // which would corrupt the defmt stream. Consume and print those lines first.
    if opts.uart_pty {
        let mut reader = std::io::BufReader::new(&mut stdout);
        let mut line = String::new();
        reader.read_line(&mut line)?;
        eprint!("{}", line);
    }

    let mut decoder = table.new_stream_decoder();

    let mut readbuf = [0; 256];
    let exit_code;
    loop {
        let n = stdout.read(&mut readbuf)?;
        decoder.received(&readbuf[..n]);
        decode_and_print(decoder.as_mut(), &current_dir, &locs)?;

        if let Some(status) = child.0.try_wait()? {
            // process finished - grab all remaining bytes and quit
            exit_code = status.code();
            let mut data = Vec::new();
            stdout.read_to_end(&mut data)?;
            decoder.received(&data);
            decode_and_print(decoder.as_mut(), &current_dir, &locs)?;
            break;
        }
    }

    // pass back qemu exit code (if any)
    Ok(exit_code)
}

/// Pump the decoder and print any new frames
fn decode_and_print(
    decoder: &mut dyn StreamDecoder,
    current_dir: &std::path::Path,
    locs: &Option<Locations>,
) -> Result<(), DecodeError> {
    loop {
        match decoder.decode() {
            Ok(frame) => {
                let (file, line, mod_path) = location_info(locs, &frame, current_dir);
                defmt_decoder::log::log_defmt(&frame, file.as_deref(), line, mod_path.as_deref());
            }
            Err(DecodeError::UnexpectedEof) => return Ok(()),
            Err(DecodeError::Malformed) => {
                eprintln!("failed to decode defmt data");
                return Err(DecodeError::Malformed);
            }
        }
    }
}

/// Describes the file, line and module a log message came from
type LocationInfo = (Option<String>, Option<u32>, Option<String>);

/// Get location info for this log message
fn location_info(
    locs: &Option<Locations>,
    frame: &Frame,
    current_dir: &std::path::Path,
) -> LocationInfo {
    let (mut file, mut line, mut mod_path) = (None, None, None);

    let loc = locs.as_ref().map(|locs| locs.get(&frame.index()));

    if let Some(Some(loc)) = loc {
        // try to get the relative path, else the full one
        let path = loc.file.strip_prefix(current_dir).unwrap_or(&loc.file);

        file = Some(path.display().to_string());
        line = Some(loc.line as u32);
        mod_path = Some(loc.module.clone());
    }

    (file, line, mod_path)
}

/// Wrapper to ensure qemu is cleaned up at the end
struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        self.0.kill().ok();
    }
}

/// Report version from Cargo.toml _(e.g. "0.1.4")_ and supported `defmt`-versions.
///
/// Used by `--version` flag.
#[allow(clippy::unnecessary_wraps)]
fn print_version() -> anyhow::Result<Option<i32>> {
    println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
    let s = if defmt_decoder::DEFMT_VERSIONS.len() > 1 {
        "s"
    } else {
        ""
    };
    println!(
        "supported defmt version{s}: {}",
        defmt_decoder::DEFMT_VERSIONS.join(", ")
    );
    Ok(Some(0))
}

/// Dumps UTF-8 data received on a socket to stdout, line by line
fn print_loop(socket: std::net::TcpListener) {
    for conn in socket.incoming().flatten() {
        conn.set_read_timeout(Some(std::time::Duration::from_millis(100)))
            .expect("Setting socket timeout");
        let mut reader = std::io::BufReader::new(conn);
        loop {
            let mut buffer = String::new();
            let Ok(_len) = reader.read_line(&mut buffer) else {
                break;
            };
            if !buffer.is_empty() {
                log::info!("UART got: {:?}", buffer);
            }
        }
    }
}
