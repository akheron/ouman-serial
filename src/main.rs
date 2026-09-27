use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, bail};
use clap::{Args, Parser, Subcommand};
use log::{info, warn};
use serde_json::{Map, Value, json};

use ouman_serial::client::Client;
use ouman_serial::config::Config;
use ouman_serial::http;
use ouman_serial::server::{self, Outcome, ResultKind, Status};

/// Controls an Ouman EH-800B heating controller through its GSM modem port.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// The configuration file [default: /etc/ouman-serial.toml, if it exists]
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the server: emulate the modem on the serial port and serve the HTTP API
    Serve {
        /// The serial device, for example /dev/ttyUSB0
        #[arg(long)]
        serial_port: Option<String>,
        /// The listen address, for example 127.0.0.1:8765
        #[arg(long)]
        listen: Option<String>,
        /// Log all serial traffic
        #[arg(long)]
        trace: bool,
    },
    /// Send any text to the controller
    Send {
        #[command(flatten)]
        client: ClientArgs,
        /// Do not wait for the reply
        #[arg(long)]
        no_wait: bool,
        text: String,
    },
    /// Send a keyword and show the reply, for example "L1 asetusarvot"
    Get {
        #[command(flatten)]
        client: ClientArgs,
        keyword: String,
    },
    /// Change fields, for example: set "L1 asetusarvot" "Menoveden minimiraja=26.0"
    Set {
        #[command(flatten)]
        client: ClientArgs,
        /// Do not wait for the reply
        #[arg(long)]
        no_wait: bool,
        keyword: String,
        /// LABEL=VALUE, with the label as the controller writes it
        #[arg(required = true)]
        fields: Vec<String>,
    },
    /// Select an option, for example: select "L1 ohjaustavat" Automaatti
    Select {
        #[command(flatten)]
        client: ClientArgs,
        /// Do not wait for the reply
        #[arg(long)]
        no_wait: bool,
        keyword: String,
        option: String,
    },
    /// Show the state of the server and the controller link
    Status {
        #[command(flatten)]
        client: ClientArgs,
    },
}

#[derive(Args)]
struct ClientArgs {
    /// The server address
    #[arg(
        long,
        env = "OUMAN_SERIAL_URL",
        default_value = "http://127.0.0.1:8765"
    )]
    server: String,
    /// The bearer token [default: the token in the configuration file, if it is readable]
    #[arg(long, env = "OUMAN_SERIAL_TOKEN", hide_env_values = true)]
    token: Option<String>,
    /// Print the full response as JSON
    #[arg(long)]
    json: bool,
}

impl ClientArgs {
    fn client(&self, config: Option<&Path>) -> Client {
        let token = self
            .token
            .clone()
            .or_else(|| Config::load(config).ok().and_then(|c| c.token));
        Client::new(&self.server, token)
    }
}

fn main() -> ExitCode {
    let Cli { config, command } = Cli::parse();
    match run(config.as_deref(), command) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::from(1)
        }
    }
}

fn run(config: Option<&Path>, command: Command) -> anyhow::Result<ExitCode> {
    match command {
        Command::Serve {
            serial_port,
            listen,
            trace,
        } => {
            serve(config, serial_port, listen, trace)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Send {
            client,
            no_wait,
            text,
        } => {
            let outcome = client
                .client(config)
                .post("/v1/send", &json!({ "text": text, "no_wait": no_wait }))?;
            Ok(report(&outcome, client.json, false))
        }
        Command::Get { client, keyword } => {
            let outcome = client
                .client(config)
                .post("/v1/get", &json!({ "keyword": keyword }))?;
            Ok(report(&outcome, client.json, true))
        }
        Command::Set {
            client,
            no_wait,
            keyword,
            fields,
        } => {
            let mut map = Map::new();
            for field in fields {
                let Some((label, value)) = field.split_once('=') else {
                    bail!("{field:?} is not in the form LABEL=VALUE");
                };
                map.insert(label.to_string(), Value::String(value.to_string()));
            }
            let body = json!({ "keyword": keyword, "fields": map, "no_wait": no_wait });
            let outcome = client.client(config).post("/v1/set", &body)?;
            Ok(report(&outcome, client.json, false))
        }
        Command::Select {
            client,
            no_wait,
            keyword,
            option,
        } => {
            let body = json!({ "keyword": keyword, "option": option, "no_wait": no_wait });
            let outcome = client.client(config).post("/v1/select", &body)?;
            Ok(report(&outcome, client.json, false))
        }
        Command::Status { client } => {
            let status = client.client(config).status()?;
            if client.json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                print_status(&status);
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// Prints the outcome and returns the exit code. `needs_reply` makes a result without a
/// reply an error.
fn report(outcome: &Outcome, json: bool, needs_reply: bool) -> ExitCode {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(outcome).expect("an outcome is always valid JSON")
        );
    } else {
        let description = match outcome.result {
            ResultKind::Confirmed => "confirmed",
            ResultKind::Received if needs_reply => "received, but no reply",
            ResultKind::Received => "received, not confirmed",
            ResultKind::Rejected => "rejected",
            ResultKind::NotReceived => "not received: the controller did not take the message",
            ResultKind::NotPolling => "not sent: the controller does not poll for messages",
        };
        println!("{description}");
        if let Some(reply) = &outcome.reply {
            println!("{}", reply.raw);
        }
        for warning in &outcome.warnings {
            eprintln!("warning: {warning}");
        }
    }
    ExitCode::from(match outcome.result {
        ResultKind::Confirmed => 0,
        ResultKind::Received if needs_reply => 1,
        ResultKind::Received => 0,
        ResultKind::Rejected => 2,
        ResultKind::NotReceived => 3,
        ResultKind::NotPolling => 4,
    })
}

fn print_status(status: &Status) {
    let never = || "never".to_string();
    println!(
        "serial port:      {} ({})",
        status.serial_port.as_deref().unwrap_or("-"),
        if status.port_open { "open" } else { "closed" }
    );
    println!(
        "last poll:        {}",
        status
            .last_poll
            .zip(status.last_poll_age_s)
            .map_or_else(never, |(time, age)| format!(
                "{} ({age:.1} s ago)",
                time.format("%Y-%m-%d %H:%M:%S")
            ))
    );
    println!(
        "last modem init:  {}",
        status
            .last_init
            .map_or_else(never, |time| time.format("%Y-%m-%d %H:%M:%S").to_string())
    );
    println!("queue:            {}", status.queue_length);
    println!(
        "SMS sent today:   {} of {} (estimate)",
        status.sms_budget.used, status.sms_budget.limit
    );
    println!("uptime:           {} s", status.uptime_s);
}

fn serve(
    config_path: Option<&Path>,
    serial_port: Option<String>,
    listen: Option<String>,
    trace: bool,
) -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let mut config = Config::load(config_path)?;
    if serial_port.is_some() {
        config.serial_port = serial_port;
    }
    if let Some(listen) = listen {
        config.listen = listen;
    }
    config.trace |= trace;
    config.validate()?;
    let path = config.serial_port.clone().context(
        "no serial port: set serial_port in the configuration file, or use --serial-port",
    )?;
    if config.token.is_none() && !is_loopback(&config.listen) {
        warn!(
            "no token, but the listen address {} is not a loopback address: \
             anyone who can connect to it can change the controller settings",
            config.listen
        );
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let config = Arc::new(config);
        let (server, link) = server::start(config.clone());
        tokio::spawn(link.run_serial(path));
        let listener = tokio::net::TcpListener::bind(&config.listen)
            .await
            .with_context(|| format!("cannot listen on {}", config.listen))?;
        info!("listening on {}", config.listen);
        axum::serve(listener, http::router(Arc::new(server)))
            .with_graceful_shutdown(shutdown())
            .await?;
        Ok(())
    })
}

fn is_loopback(listen: &str) -> bool {
    match listen.parse::<SocketAddr>() {
        Ok(address) => address.ip().is_loopback(),
        Err(_) => listen.starts_with("localhost:"),
    }
}

async fn shutdown() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut terminate =
        signal(SignalKind::terminate()).expect("cannot install the SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = terminate.recv() => {}
    }
    info!("stopping");
}
