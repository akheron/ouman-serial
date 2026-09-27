//! Tests of the server with a simulated controller. The simulator initializes the modem and
//! polls for messages like the real controller, and replies to the messages that it reads.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request as HttpRequest, StatusCode, header};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::sync::mpsc;
use tower::ServiceExt;

use crate::config::{Config, Timeouts};
use crate::http;
use crate::message::{self, Segment};
use crate::pdu::controller::{decode_deliver, encode_submit};
use crate::pdu::{from_hex, to_hex};
use crate::server::{self, Kind, Outcome, Request, ResultKind, Server, SubmitError};

#[derive(Clone, Copy, PartialEq)]
enum Behaviour {
    Normal,
    /// Takes messages, but the daily limit blocks all replies.
    NoReply,
    /// Polls, but never takes a message.
    IgnoreMessages,
    /// Initializes the modem, but never polls.
    NoPolling,
    /// Replies with other values than requested.
    WrongValue,
}

struct Sms {
    to: String,
    text: String,
}

struct Controller {
    stream: DuplexStream,
    behaviour: Behaviour,
    received: Arc<Mutex<Vec<String>>>,
    sms: mpsc::UnboundedReceiver<Sms>,
    reference: u8,
}

impl Controller {
    async fn run(mut self) {
        for command in [
            "AT",
            "AT&F",
            "ATE0",
            "AT+CPIN?",
            "AT+CMGF=0",
            "AT+CNMI=0,0,0,0",
            "AT+CPMS=\"SM\",\"SM\",\"SM\"",
        ] {
            self.command(command).await;
        }
        loop {
            while let Ok(sms) = self.sms.try_recv() {
                self.send_sms(&sms.to, &sms.text).await;
            }
            if self.behaviour != Behaviour::NoPolling {
                self.poll().await;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn poll(&mut self) {
        let listing = self.command("AT+CMGL=4").await;
        let Some(pdu) = listing
            .lines()
            .skip_while(|l| !l.starts_with("+CMGL:"))
            .nth(1)
        else {
            return;
        };
        if self.behaviour == Behaviour::IgnoreMessages {
            return;
        }
        self.command("AT+CMGD=1").await;
        let (sender, text) = decode_deliver(&from_hex(pdu.trim()).unwrap());
        self.received.lock().unwrap().push(text.clone());
        if let Some(reply) = reply_to(&text, self.behaviour) {
            self.send_sms(&sender, &reply).await;
        }
    }

    async fn command(&mut self, command: &str) -> String {
        self.stream
            .write_all(format!("{command}\r").as_bytes())
            .await
            .unwrap();
        self.read_response().await
    }

    async fn read_response(&mut self) -> String {
        let mut response = String::new();
        let mut buffer = [0u8; 512];
        loop {
            let read = tokio::time::timeout(Duration::from_secs(2), self.stream.read(&mut buffer));
            let n = read.await.expect("the modem did not answer").unwrap();
            assert!(n > 0, "the modem closed the connection");
            response.push_str(std::str::from_utf8(&buffer[..n]).unwrap());
            if response.ends_with("\r\nOK\r\n")
                || response.ends_with("\r\n> ")
                || response.contains("ERROR")
            {
                return response;
            }
        }
    }

    /// Sends a text like the real controller: longer texts are cut into concatenated parts
    /// of 153 characters, also in the middle of a word.
    async fn send_sms(&mut self, to: &str, text: &str) {
        let chars: Vec<char> = text.chars().collect();
        let parts: Vec<String> = if chars.len() <= 160 {
            vec![text.to_string()]
        } else {
            chars
                .chunks(153)
                .map(|chunk| chunk.iter().collect())
                .collect()
        };
        self.reference = self.reference.wrapping_add(1);
        let total = parts.len() as u8;
        for (index, part) in parts.iter().enumerate() {
            let concat = (total > 1).then_some((self.reference, total, index as u8 + 1));
            let pdu = encode_submit(to, part, concat);
            let prompt = self.command(&format!("AT+CMGS={}", pdu.len() - 1)).await;
            assert_eq!(prompt, "\r\n> ");
            self.stream
                .write_all(format!("{}\x1a", to_hex(&pdu)).as_bytes())
                .await
                .unwrap();
            let response = self.read_response().await;
            assert!(response.contains("+CMGS:"), "{response:?}");
            self.stream.write_all(b"\r").await.unwrap();
        }
    }
}

fn reply_to(text: &str, behaviour: Behaviour) -> Option<String> {
    if behaviour == Behaviour::NoReply {
        return None;
    }
    let Some((title, body)) = text.split_once(':') else {
        return Some(match text.to_uppercase().as_str() {
            "LONG" => long_reply(),
            keyword => format!("{keyword}: Ulkolämpötila=11.5/ L1 Normaalilämpö"),
        });
    };
    let body = body.trim();
    if let Some(option) = body.strip_prefix('*') {
        let selected = if behaviour == Behaviour::WrongValue {
            "Automaatti"
        } else {
            option
        };
        return Some(format!("{title}: *{selected}/ Lämmönpudotus"));
    }
    if behaviour == Behaviour::WrongValue {
        let fields: Vec<String> = body
            .split("/ ")
            .map(|field| format!("{}=99.0", field.split('=').next().unwrap()))
            .collect();
        return Some(format!("{title}: {}", fields.join("/ ")));
    }
    Some(text.to_string())
}

fn long_reply() -> String {
    let fields: Vec<String> = (1..=30).map(|i| format!("Kenttä {i}={i}.0")).collect();
    format!("LONG: {}", fields.join("/ "))
}

struct Harness {
    server: Arc<Server>,
    received: Arc<Mutex<Vec<String>>>,
    sms: mpsc::UnboundedSender<Sms>,
}

async fn start(behaviour: Behaviour, adjust: impl FnOnce(&mut Config)) -> Harness {
    let mut config = Config {
        timeouts: Timeouts {
            receive_s: 1.0,
            reply_s: 0.5,
            poll_silence_s: 0.5,
        },
        ..Config::default()
    };
    adjust(&mut config);
    let (server, mut link) = server::start(Arc::new(config));
    let (ours, theirs) = tokio::io::duplex(4096);
    tokio::spawn(async move { link.run_stream(ours).await });
    let received = Arc::default();
    let (sms_tx, sms_rx) = mpsc::unbounded_channel();
    let controller = Controller {
        stream: theirs,
        behaviour,
        received: Arc::clone(&received),
        sms: sms_rx,
        reference: 0,
    };
    tokio::spawn(controller.run());
    let server = Arc::new(server);
    let ready = async {
        loop {
            let status = server.status();
            let ready = match behaviour {
                Behaviour::NoPolling => status.last_init.is_some(),
                _ => status.last_poll.is_some(),
            };
            if ready {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(2), ready)
        .await
        .expect("the simulated controller did not start");
    Harness {
        server,
        received,
        sms: sms_tx,
    }
}

fn request(text: &str, kind: Kind) -> Request {
    Request {
        text: text.into(),
        kind,
        wait_reply: true,
    }
}

async fn deliver(server: &Server, request: Request) -> Outcome {
    server.submit(request).unwrap().await.unwrap()
}

fn fields(items: &[(&str, &str)]) -> Vec<(String, String)> {
    items
        .iter()
        .map(|&(l, v)| (l.to_string(), v.to_string()))
        .collect()
}

async fn set(server: &Server, keyword: &str, items: &[(&str, &str)]) -> Outcome {
    let fields = fields(items);
    let text = message::build_set(keyword, &fields).unwrap();
    deliver(server, request(&text, Kind::Set(fields))).await
}

async fn select(server: &Server, keyword: &str, option: &str) -> Outcome {
    let text = message::build_select(keyword, option).unwrap();
    deliver(server, request(&text, Kind::Select(option.into()))).await
}

#[tokio::test]
async fn get_returns_the_parsed_reply() {
    let h = start(Behaviour::Normal, |_| {}).await;
    let outcome = deliver(&h.server, request("Ouman", Kind::Get)).await;
    assert_eq!(outcome.result, ResultKind::Confirmed);
    let reply = outcome.reply.unwrap();
    assert_eq!(reply.title, "OUMAN");
    assert_eq!(
        reply.segments[0],
        Segment::Field {
            label: "Ulkolämpötila".into(),
            value: "11.5".into()
        }
    );
    assert_eq!(h.server.status().sms_budget.used, 1);
}

#[tokio::test]
async fn set_and_select_are_confirmed_by_the_reply() {
    let h = start(Behaviour::Normal, |_| {}).await;
    let outcome = set(
        &h.server,
        "L1 asetusarvot",
        &[("Menoveden minimiraja", "26")],
    )
    .await;
    assert_eq!(
        outcome.result,
        ResultKind::Confirmed,
        "{:?}",
        outcome.warnings
    );
    let outcome = select(&h.server, "L1 ohjaustavat", "Jatkuva normaalilämpö").await;
    assert_eq!(
        outcome.result,
        ResultKind::Confirmed,
        "{:?}",
        outcome.warnings
    );
    assert_eq!(
        *h.received.lock().unwrap(),
        vec![
            "L1 ASETUSARVOT: Menoveden minimiraja=26",
            "L1 OHJAUSTAVAT: *Jatkuva normaalilämpö"
        ]
    );
}

#[tokio::test]
async fn a_reply_with_other_values_is_rejected() {
    let h = start(Behaviour::WrongValue, |_| {}).await;
    let outcome = set(
        &h.server,
        "L1 asetusarvot",
        &[("Menoveden minimiraja", "26")],
    )
    .await;
    assert_eq!(outcome.result, ResultKind::Rejected);
    assert_eq!(
        outcome.warnings,
        vec![r#""Menoveden minimiraja" is "99.0" in the reply, requested "26""#]
    );
    let outcome = select(&h.server, "L1 ohjaustavat", "Lämmönpudotus").await;
    assert_eq!(outcome.result, ResultKind::Rejected);
}

#[tokio::test]
async fn without_a_reply_the_result_is_received() {
    let h = start(Behaviour::NoReply, |_| {}).await;
    let outcome = set(
        &h.server,
        "L1 asetusarvot",
        &[("Menoveden minimiraja", "26")],
    )
    .await;
    assert_eq!(outcome.result, ResultKind::Received);
    assert!(outcome.reply.is_none());
    assert_eq!(outcome.warnings.len(), 1);
}

#[tokio::test]
async fn no_wait_returns_when_the_controller_takes_the_message() {
    let h = start(Behaviour::Normal, |_| {}).await;
    let mut request = request("Ouman", Kind::Send);
    request.wait_reply = false;
    let outcome = deliver(&h.server, request).await;
    assert_eq!(outcome.result, ResultKind::Received);
    assert!(outcome.warnings.is_empty());
}

#[tokio::test]
async fn a_message_that_the_controller_does_not_take_is_withdrawn() {
    let h = start(Behaviour::IgnoreMessages, |_| {}).await;
    let outcome = deliver(&h.server, request("Ouman", Kind::Get)).await;
    assert_eq!(outcome.result, ResultKind::NotReceived);
    // After the withdrawal, the next request must not find the old message in the inbox.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(h.received.lock().unwrap().is_empty());
}

#[tokio::test]
async fn nothing_is_sent_when_the_controller_does_not_poll() {
    let h = start(Behaviour::NoPolling, |_| {}).await;
    let outcome = deliver(&h.server, request("Ouman", Kind::Get)).await;
    assert_eq!(outcome.result, ResultKind::NotPolling);
}

#[tokio::test]
async fn joins_a_concatenated_reply() {
    let h = start(Behaviour::Normal, |_| {}).await;
    let outcome = deliver(&h.server, request("long", Kind::Get)).await;
    assert_eq!(outcome.result, ResultKind::Confirmed);
    assert_eq!(outcome.reply.unwrap().raw, long_reply());
    let parts = long_reply().chars().count().div_ceil(153) as u32;
    assert!(parts > 2);
    assert_eq!(h.server.status().sms_budget.used, parts);
}

#[tokio::test]
async fn queued_requests_go_one_at_a_time_and_get_their_own_replies() {
    let h = start(Behaviour::Normal, |_| {}).await;
    let first = h.server.submit(request("first", Kind::Get)).unwrap();
    let second = h.server.submit(request("second", Kind::Get)).unwrap();
    assert_eq!(first.await.unwrap().reply.unwrap().title, "FIRST");
    assert_eq!(second.await.unwrap().reply.unwrap().title, "SECOND");
}

#[tokio::test]
async fn a_request_whose_client_left_the_queue_is_not_sent() {
    let h = start(Behaviour::Normal, |_| {}).await;
    let first = h.server.submit(request("first", Kind::Get)).unwrap();
    drop(h.server.submit(request("second", Kind::Get)).unwrap());
    let third = h.server.submit(request("third", Kind::Get)).unwrap();
    first.await.unwrap();
    third.await.unwrap();
    assert_eq!(*h.received.lock().unwrap(), vec!["first", "third"]);
}

#[tokio::test]
async fn a_full_queue_refuses_new_requests() {
    let h = start(Behaviour::Normal, |config| config.queue_max = 1).await;
    let _first = h.server.submit(request("first", Kind::Get)).unwrap();
    assert_eq!(
        h.server.submit(request("second", Kind::Get)).err(),
        Some(SubmitError::QueueFull)
    );
}

#[tokio::test]
async fn refuses_a_message_that_does_not_fit_in_one_sms() {
    let h = start(Behaviour::Normal, |config| {
        config.device_id = Some("TC01".into())
    })
    .await;
    // With the device ID and a space, 156 characters become 161.
    let result = h.server.submit(request(&"x".repeat(156), Kind::Send));
    assert!(matches!(result, Err(SubmitError::Invalid(_))));
    assert!(
        h.server
            .submit(request(&"x".repeat(155), Kind::Send))
            .is_ok()
    );
}

#[tokio::test]
async fn the_device_id_goes_before_the_message() {
    let h = start(Behaviour::Normal, |config| {
        config.device_id = Some("TC01".into())
    })
    .await;
    deliver(&h.server, request("Ouman", Kind::Send)).await;
    assert_eq!(*h.received.lock().unwrap(), vec!["TC01 Ouman"]);
}

#[tokio::test]
async fn an_alarm_runs_the_hook() {
    let path = std::env::temp_dir().join(format!("ouman-serial-alarm-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let hook = format!("cat > {}", path.display());
    let h = start(Behaviour::Normal, |config| config.alarm_hook = Some(hook)).await;
    h.sms
        .send(Sms {
            to: "+358401111111".into(),
            text: "HÄLYTYS: Menoveden lämpötila=50.0/ MA 23.3.2009 13:31".into(),
        })
        .unwrap_or_else(|_| panic!("the simulator stopped"));
    let written = async {
        loop {
            if let Ok(text) = std::fs::read_to_string(&path)
                && text.ends_with('}')
            {
                return text;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    let text = tokio::time::timeout(Duration::from_secs(3), written)
        .await
        .expect("the hook did not run");
    let _ = std::fs::remove_file(&path);
    let alarm: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(alarm["destination"], "+358401111111");
    assert_eq!(alarm["title"], "HÄLYTYS");
    assert_eq!(alarm["segments"][0]["value"], "50.0");
    assert_eq!(h.server.status().sms_budget.used, 1);
}

async fn call(app: &axum::Router, request: HttpRequest<Body>) -> (StatusCode, serde_json::Value) {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

fn post(path: &str, token: &str, body: serde_json::Value) -> HttpRequest<Body> {
    HttpRequest::post(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

#[tokio::test]
async fn the_http_api_checks_the_token_and_the_input() {
    let h = start(Behaviour::Normal, |config| {
        config.token = Some("secret".into())
    })
    .await;
    let app = http::router(h.server.clone());

    let request = HttpRequest::get("/v1/status").body(Body::empty()).unwrap();
    assert_eq!(call(&app, request).await.0, StatusCode::UNAUTHORIZED);
    let request = post("/v1/set", "wrong", serde_json::json!({}));
    assert_eq!(call(&app, request).await.0, StatusCode::UNAUTHORIZED);

    let body = serde_json::json!({ "keyword": "L1 asetusarvot", "fields": { "a/b": "1" } });
    let (status, response) = call(&app, post("/v1/set", "secret", body)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(response["error"].as_str().unwrap().contains("a/b"));

    let body = serde_json::json!({ "keyword": "L1 asetusarvot", "fields": { "Menoveden minimiraja": 26 } });
    let (status, response) = call(&app, post("/v1/set", "secret", body)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(response["result"], "confirmed");
    assert_eq!(response["reply"]["segments"][0]["type"], "field");

    let request = HttpRequest::get("/v1/status")
        .header(header::AUTHORIZATION, "Bearer secret")
        .body(Body::empty())
        .unwrap();
    let (status, response) = call(&app, request).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(response["sms_budget"]["used"], 1);
}
