//! The server core: the serial link with the emulated modem, the shared state, and the
//! dispatcher that delivers one message at a time.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

use chrono::{DateTime, Local, NaiveDate};
use log::{debug, info, warn};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;
use tokio_serial::{DataBits, FlowControl, Parity, SerialPortBuilderExt, SerialStream, StopBits};

use crate::config::Config;
use crate::message::{self, Reply};
use crate::modem::{Event, InboxMessage, Modem};
use crate::pdu;

const TRACE_TARGET: &str = "ouman_serial::trace";
const PART_LIFETIME: Duration = Duration::from_secs(300);
const REOPEN_DELAY: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultKind {
    Confirmed,
    Received,
    Rejected,
    NotReceived,
    NotPolling,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Outcome {
    pub result: ResultKind,
    pub warnings: Vec<String>,
    pub reply: Option<Reply>,
}

#[derive(Debug, Clone)]
pub enum Kind {
    Send,
    Get,
    Set(Vec<(String, String)>),
    Select(String),
}

#[derive(Debug, Clone)]
pub struct Request {
    pub text: String,
    pub kind: Kind,
    pub wait_reply: bool,
}

#[derive(Debug, PartialEq)]
pub enum SubmitError {
    Invalid(String),
    QueueFull,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Status {
    pub serial_port: Option<String>,
    pub port_open: bool,
    pub last_poll: Option<DateTime<Local>>,
    pub last_poll_age_s: Option<f64>,
    pub last_init: Option<DateTime<Local>>,
    pub queue_length: usize,
    pub sms_budget: BudgetStatus,
    pub uptime_s: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BudgetStatus {
    pub date: NaiveDate,
    pub used: u32,
    pub limit: u32,
}

pub enum LinkCommand {
    Put(InboxMessage),
    Withdraw(u64),
}

struct Job {
    request: Request,
    respond: oneshot::Sender<Outcome>,
}

pub struct Server {
    hub: Arc<Hub>,
    jobs: mpsc::Sender<Job>,
}

pub struct Link {
    hub: Arc<Hub>,
    modem: Modem,
    commands: mpsc::UnboundedReceiver<LinkCommand>,
}

/// Starts the dispatcher. The caller must run the returned link.
pub fn start(config: Arc<Config>) -> (Server, Link) {
    let hub = Arc::new(Hub::new(config.clone()));
    let (jobs_tx, jobs_rx) = mpsc::channel(config.queue_max);
    let (commands_tx, commands_rx) = mpsc::unbounded_channel();
    tokio::spawn(dispatch(hub.clone(), commands_tx, jobs_rx));
    let link = Link {
        hub: hub.clone(),
        modem: Modem::new(&config.smsc),
        commands: commands_rx,
    };
    (Server { hub, jobs: jobs_tx }, link)
}

impl Server {
    pub fn config(&self) -> &Config {
        &self.hub.config
    }

    /// Queues the request. The receiver gets the outcome. If the caller drops the receiver
    /// before the request starts, the request is not sent.
    pub fn submit(&self, request: Request) -> Result<oneshot::Receiver<Outcome>, SubmitError> {
        pdu::septet_count(&self.hub.message_text(&request.text))
            .map_err(|e| SubmitError::Invalid(e.0))?;
        let (respond, outcome) = oneshot::channel();
        self.jobs
            .try_send(Job { request, respond })
            .map_err(|_| SubmitError::QueueFull)?;
        Ok(outcome)
    }

    pub fn status(&self) -> Status {
        let queue_length = self.jobs.max_capacity() - self.jobs.capacity();
        self.hub.status(queue_length)
    }
}

impl Link {
    /// Keeps the serial port open, and opens it again after an error.
    pub async fn run_serial(mut self, path: String) {
        let mut last_error = None;
        loop {
            match open_port(&path) {
                Ok(stream) => {
                    info!("opened {path}");
                    last_error = None;
                    let error = self.run_stream(stream).await;
                    warn!("closed {path}: {error}");
                }
                Err(error) => {
                    let message = error.to_string();
                    if last_error.as_ref() != Some(&message) {
                        warn!("cannot open {path}: {message}");
                    }
                    last_error = Some(message);
                }
            }
            let delay = tokio::time::sleep(REOPEN_DELAY);
            tokio::pin!(delay);
            loop {
                tokio::select! {
                    _ = &mut delay => break,
                    Some(command) = self.commands.recv() => self.apply(command),
                }
            }
        }
    }

    /// Runs the modem on one connection until it fails.
    pub async fn run_stream<S: AsyncRead + AsyncWrite>(&mut self, stream: S) -> io::Error {
        let (mut reader, mut writer) = tokio::io::split(stream);
        let trace = self.hub.config.trace;
        self.hub.state().port_open = true;
        let mut buffer = [0u8; 512];
        let error = loop {
            tokio::select! {
                read = reader.read(&mut buffer) => {
                    let data = match read {
                        Ok(0) => break io::Error::new(io::ErrorKind::UnexpectedEof, "end of stream"),
                        Ok(n) => &buffer[..n],
                        Err(error) => break error,
                    };
                    if trace {
                        info!(target: TRACE_TARGET, "RX {}", escape(data));
                    }
                    let output = self.modem.feed(data);
                    if !output.bytes.is_empty() {
                        if trace {
                            info!(target: TRACE_TARGET, "TX {}", escape(&output.bytes));
                        }
                        if let Err(error) = writer.write_all(&output.bytes).await {
                            break error;
                        }
                    }
                    for event in output.events {
                        self.hub.handle(event);
                    }
                }
                Some(command) = self.commands.recv() => self.apply(command),
            }
        };
        self.hub.state().port_open = false;
        error
    }

    fn apply(&mut self, command: LinkCommand) {
        match command {
            LinkCommand::Put(message) => self.modem.put(message),
            LinkCommand::Withdraw(id) => {
                if self.modem.withdraw(id) {
                    debug!("withdrew message {id}");
                }
            }
        }
    }
}

fn open_port(path: &str) -> io::Result<SerialStream> {
    let port = tokio_serial::new(path, 9600)
        .data_bits(DataBits::Eight)
        .parity(Parity::None)
        .stop_bits(StopBits::One)
        .flow_control(FlowControl::None)
        .open_native_async()?;
    Ok(port)
}

struct Hub {
    config: Arc<Config>,
    started: Instant,
    state: Mutex<HubState>,
}

struct HubState {
    port_open: bool,
    last_poll: Option<(Instant, DateTime<Local>)>,
    last_init: Option<DateTime<Local>>,
    budget_date: NaiveDate,
    budget_used: u32,
    current: Option<Current>,
    parts: HashMap<(String, u16, u8), Parts>,
}

struct Current {
    id: u64,
    sender: String,
    deleted: Option<oneshot::Sender<()>>,
    reply: Option<oneshot::Sender<String>>,
}

struct Parts {
    started: Instant,
    texts: BTreeMap<u8, String>,
}

impl Hub {
    fn new(config: Arc<Config>) -> Self {
        Self {
            config,
            started: Instant::now(),
            state: Mutex::new(HubState {
                port_open: false,
                last_poll: None,
                last_init: None,
                budget_date: Local::now().date_naive(),
                budget_used: 0,
                current: None,
                parts: HashMap::new(),
            }),
        }
    }

    fn state(&self) -> MutexGuard<'_, HubState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn message_text(&self, text: &str) -> String {
        match self.config.device_id.as_deref() {
            Some(id) if !id.is_empty() => format!("{id} {text}"),
            _ => text.to_string(),
        }
    }

    fn is_sender_number(&self, number: &str) -> bool {
        number.len() == self.config.sender_prefix.len() + 4
            && number.starts_with(&self.config.sender_prefix)
    }

    fn handle(&self, event: Event) {
        match event {
            Event::Polled => self.state().last_poll = Some((Instant::now(), Local::now())),
            Event::Initialized => {
                debug!("the controller initializes the modem");
                self.state().last_init = Some(Local::now());
            }
            Event::Listed(id) => debug!("the controller read message {id}"),
            Event::Deleted(id) => {
                let mut state = self.state();
                match state
                    .current
                    .as_mut()
                    .filter(|c| c.id == id)
                    .and_then(|c| c.deleted.take())
                {
                    Some(deleted) => {
                        let _ = deleted.send(());
                    }
                    None => {
                        debug!("the controller deleted message {id}, which has no waiting request")
                    }
                }
            }
            Event::Submitted(pdu) => self.submitted(&pdu),
        }
    }

    fn submitted(&self, raw: &[u8]) {
        let mut state = self.state();
        state.count_budget();
        let submit = match pdu::decode_submit(raw) {
            Ok(submit) => submit,
            Err(error) => {
                warn!(
                    "cannot decode an SMS from the controller ({error}): {}",
                    pdu::to_hex(raw)
                );
                return;
            }
        };
        state
            .parts
            .retain(|_, parts| parts.started.elapsed() < PART_LIFETIME);
        let complete = match submit.concat {
            Some(concat) if concat.total > 1 => {
                let key = (submit.destination.clone(), concat.reference, concat.total);
                let parts = state.parts.entry(key.clone()).or_insert_with(|| Parts {
                    started: Instant::now(),
                    texts: BTreeMap::new(),
                });
                parts.texts.insert(concat.part, submit.text);
                if parts.texts.len() < usize::from(concat.total) {
                    return;
                }
                let parts = state
                    .parts
                    .remove(&key)
                    .expect("the entry was just updated");
                parts.texts.into_values().collect()
            }
            _ => submit.text,
        };
        let destination = submit.destination;
        if let Some(reply) = state
            .current
            .as_mut()
            .filter(|c| c.sender == destination)
            .and_then(|c| c.reply.take())
        {
            let _ = reply.send(complete);
        } else if self.is_sender_number(&destination) {
            info!("late reply to {destination}: {complete}");
        } else {
            drop(state);
            self.alarm(destination, complete);
        }
    }

    fn alarm(&self, destination: String, text: String) {
        warn!("alarm SMS to {destination}: {text}");
        let Some(hook) = self.config.alarm_hook.clone() else {
            return;
        };
        let reply = message::parse(&text);
        let input = serde_json::json!({
            "destination": destination,
            "raw": reply.raw,
            "title": reply.title,
            "segments": reply.segments,
            "time": Local::now(),
        });
        tokio::spawn(run_hook(hook, input.to_string()));
    }

    fn status(&self, queue_length: usize) -> Status {
        let mut state = self.state();
        let used = state.budget_used();
        Status {
            serial_port: self.config.serial_port.clone(),
            port_open: state.port_open,
            last_poll: state.last_poll.map(|(_, time)| time),
            last_poll_age_s: state
                .last_poll
                .map(|(instant, _)| instant.elapsed().as_secs_f64()),
            last_init: state.last_init,
            queue_length,
            sms_budget: BudgetStatus {
                date: state.budget_date,
                used,
                limit: self.config.daily_limit,
            },
            uptime_s: self.started.elapsed().as_secs(),
        }
    }
}

impl HubState {
    fn budget_used(&mut self) -> u32 {
        let today = Local::now().date_naive();
        if self.budget_date != today {
            self.budget_date = today;
            self.budget_used = 0;
        }
        self.budget_used
    }

    fn count_budget(&mut self) {
        self.budget_used();
        self.budget_used += 1;
    }
}

async fn run_hook(command: String, input: String) {
    let child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(&command)
        .stdin(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(error) => {
            warn!("cannot run the alarm hook {command:?}: {error}");
            return;
        }
    };
    if let Some(mut stdin) = child.stdin.take()
        && let Err(error) = stdin.write_all(input.as_bytes()).await
    {
        warn!("cannot write to the alarm hook: {error}");
    }
    match child.wait().await {
        Ok(status) if !status.success() => warn!("the alarm hook failed: {status}"),
        Err(error) => warn!("the alarm hook failed: {error}"),
        Ok(_) => {}
    }
}

async fn dispatch(
    hub: Arc<Hub>,
    link: mpsc::UnboundedSender<LinkCommand>,
    mut jobs: mpsc::Receiver<Job>,
) {
    let mut id = 0;
    let mut sender_counter = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() % 10_000);
    while let Some(job) = jobs.recv().await {
        if job.respond.is_closed() {
            info!(
                "not sending {:?}: the client left the queue",
                job.request.text
            );
            continue;
        }
        id += 1;
        sender_counter = (sender_counter + 1) % 10_000;
        let sender = format!("{}{sender_counter:04}", hub.config.sender_prefix);
        let outcome = deliver(&hub, &link, id, sender, &job.request).await;
        info!("{:?}: {:?}", job.request.text, outcome.result);
        let _ = job.respond.send(outcome);
    }
}

/// Clears the current request when the delivery ends, also when the delivery is cancelled.
struct CurrentGuard<'a>(&'a Hub);

impl Drop for CurrentGuard<'_> {
    fn drop(&mut self) {
        self.0.state().current = None;
    }
}

async fn deliver(
    hub: &Hub,
    link: &mpsc::UnboundedSender<LinkCommand>,
    id: u64,
    sender: String,
    request: &Request,
) -> Outcome {
    let config = &hub.config;
    let mut outcome = Outcome {
        result: ResultKind::NotPolling,
        warnings: Vec::new(),
        reply: None,
    };
    if matches!(request.kind, Kind::Get) {
        let used = hub.state().budget_used();
        if used >= config.daily_limit {
            outcome.warnings.push(format!(
                "the daily SMS limit is probably used up ({used} of {}), so the controller may not reply",
                config.daily_limit
            ));
        }
    }
    let polled_recently = hub
        .state()
        .last_poll
        .is_some_and(|(instant, _)| instant.elapsed() < config.timeouts.poll_silence());
    if !polled_recently {
        return outcome;
    }
    let text = hub.message_text(&request.text);
    let deliver =
        match pdu::encode_deliver(&config.smsc, &sender, &text, Local::now().fixed_offset()) {
            Ok(deliver) => deliver,
            Err(error) => {
                warn!("cannot encode {text:?}: {error}");
                outcome.result = ResultKind::NotReceived;
                outcome.warnings.push(error.0);
                return outcome;
            }
        };
    let (deleted_tx, deleted_rx) = oneshot::channel();
    let (reply_tx, reply_rx) = oneshot::channel();
    hub.state().current = Some(Current {
        id,
        sender,
        deleted: Some(deleted_tx),
        reply: Some(reply_tx),
    });
    let _guard = CurrentGuard(hub);
    let _ = link.send(LinkCommand::Put(InboxMessage {
        id,
        pdu_hex: deliver.hex,
        tpdu_len: deliver.tpdu_len,
    }));

    if !matches!(
        timeout(config.timeouts.receive(), deleted_rx).await,
        Ok(Ok(()))
    ) {
        let _ = link.send(LinkCommand::Withdraw(id));
        outcome.result = ResultKind::NotReceived;
        return outcome;
    }
    outcome.result = ResultKind::Received;
    if !request.wait_reply {
        return outcome;
    }
    let Ok(Ok(text)) = timeout(config.timeouts.reply(), reply_rx).await else {
        outcome.warnings.push(format!(
            "no reply in {} s, so the daily SMS limit probably blocks the replies",
            config.timeouts.reply_s
        ));
        return outcome;
    };
    let reply = message::parse(&text);
    let problems = match &request.kind {
        Kind::Set(fields) => message::check_set(&reply, fields),
        Kind::Select(option) => message::check_select(&reply, option).into_iter().collect(),
        Kind::Send | Kind::Get => Vec::new(),
    };
    outcome.result = if problems.is_empty() {
        ResultKind::Confirmed
    } else {
        ResultKind::Rejected
    };
    outcome.warnings.extend(problems);
    outcome.reply = Some(reply);
    outcome
}

/// Shows printable ASCII as it is, and other bytes as escapes.
pub fn escape(data: &[u8]) -> String {
    let mut text = String::with_capacity(data.len());
    for &byte in data {
        match byte {
            b'\r' => text.push_str("\\r"),
            b'\n' => text.push_str("\\n"),
            b'\t' => text.push_str("\\t"),
            b'\\' => text.push_str("\\\\"),
            0x20..=0x7E => text.push(char::from(byte)),
            _ => text.push_str(&format!("\\x{byte:02x}")),
        }
    }
    text
}
