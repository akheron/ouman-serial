//! The emulated GSM modem: AT commands from the controller in, replies and events out.

use log::debug;

use crate::pdu;

/// A received SMS that waits in the emulated SIM for the controller to read it.
#[derive(Debug, Clone, PartialEq)]
pub struct InboxMessage {
    pub id: u64,
    pub pdu_hex: String,
    pub tpdu_len: usize,
}

#[derive(Debug, PartialEq)]
pub enum Event {
    /// The controller asked for the list of received messages.
    Polled,
    /// The controller restored the factory settings, which starts its initialization.
    Initialized,
    Listed(u64),
    Deleted(u64),
    /// The controller sent an SMS. The value is the PDU with the SMSC address.
    Submitted(Vec<u8>),
}

#[derive(Default)]
pub struct Output {
    pub bytes: Vec<u8>,
    pub events: Vec<Event>,
}

enum Mode {
    Command,
    SmsEntry,
}

const MAX_LINE: usize = 1024;
const MESSAGE_INDEX: u32 = 1;
const STORAGE_SIZE: u32 = 20;
const CTRL_Z: u8 = 0x1A;
const ESC: u8 = 0x1B;

pub struct Modem {
    smsc: String,
    mode: Mode,
    buffer: Vec<u8>,
    inbox: Option<InboxMessage>,
    message_reference: u8,
}

impl Modem {
    pub fn new(smsc: &str) -> Self {
        Self {
            smsc: smsc.to_string(),
            mode: Mode::Command,
            buffer: Vec::new(),
            inbox: None,
            message_reference: 0,
        }
    }

    pub fn put(&mut self, message: InboxMessage) {
        self.inbox = Some(message);
    }

    /// Removes the message if it is still in the inbox. Returns true if it was.
    pub fn withdraw(&mut self, id: u64) -> bool {
        if self.inbox.as_ref().is_some_and(|m| m.id == id) {
            self.inbox = None;
            true
        } else {
            false
        }
    }

    pub fn feed(&mut self, data: &[u8]) -> Output {
        let mut output = Output::default();
        for &byte in data {
            match self.mode {
                Mode::Command => match byte {
                    b'\r' => {
                        let line = std::mem::take(&mut self.buffer);
                        self.handle_line(&line, &mut output);
                    }
                    b'\n' => {}
                    _ if self.buffer.len() < MAX_LINE => self.buffer.push(byte),
                    _ => self.buffer.clear(),
                },
                Mode::SmsEntry => match byte {
                    CTRL_Z => {
                        let hex = std::mem::take(&mut self.buffer);
                        self.mode = Mode::Command;
                        self.submit(&hex, &mut output);
                    }
                    ESC => {
                        self.buffer.clear();
                        self.mode = Mode::Command;
                        output.bytes.extend(b"\r\nOK\r\n");
                    }
                    b'\r' | b'\n' => {}
                    _ if self.buffer.len() < MAX_LINE => self.buffer.push(byte),
                    _ => self.buffer.clear(),
                },
            }
        }
        output
    }

    fn handle_line(&mut self, line: &[u8], output: &mut Output) {
        let text = String::from_utf8_lossy(line);
        let command = text.trim_matches(|c: char| c.is_whitespace() || c.is_control());
        // The controller sends Ctrl-Z and CR to end any SMS entry in progress. Outside an SMS
        // entry, a real modem ignores it, and so do lines without "AT".
        if !command
            .get(..2)
            .is_some_and(|p| p.eq_ignore_ascii_case("AT"))
            || line.contains(&CTRL_Z)
        {
            return;
        }
        let upper = command.to_ascii_uppercase();
        let used = u32::from(self.inbox.is_some());
        let info: Vec<String> = match upper.as_str() {
            "AT&F" => {
                output.events.push(Event::Initialized);
                vec![]
            }
            "AT+CPIN?" => vec!["+CPIN: READY".into()],
            "AT+CPMS=?" => vec![r#"+CPMS: ("SM"),("SM"),("SM")"#.into()],
            "AT+CPMS?" => vec![format!(
                r#"+CPMS: "SM",{used},{STORAGE_SIZE},"SM",{used},{STORAGE_SIZE},"SM",{used},{STORAGE_SIZE}"#
            )],
            "AT+CSCA?" => vec![format!(r#"+CSCA: "{}",145"#, self.smsc)],
            "AT+CREG?" => vec!["+CREG: 0,1".into()],
            "AT+CSQ" => vec!["+CSQ: 20,0".into()],
            "AT+COPS?" => vec![r#"+COPS: 0,0,"ouman-serial""#.into()],
            _ if upper.starts_with("AT+CSMS=") => vec!["+CSMS: 1,1,1".into()],
            _ if upper.starts_with("AT+CPMS=") => vec![format!(
                "+CPMS: {used},{STORAGE_SIZE},{used},{STORAGE_SIZE},{used},{STORAGE_SIZE}"
            )],
            _ if upper.starts_with("AT+CMGL") => {
                output.events.push(Event::Polled);
                match &self.inbox {
                    Some(message) => {
                        output.events.push(Event::Listed(message.id));
                        vec![
                            format!("+CMGL: {MESSAGE_INDEX},0,,{}", message.tpdu_len),
                            message.pdu_hex.clone(),
                        ]
                    }
                    None => vec![],
                }
            }
            _ if upper.starts_with("AT+CMGR=") => match (&self.inbox, parse_index(&upper[8..])) {
                (Some(message), Some(MESSAGE_INDEX)) => {
                    output.events.push(Event::Listed(message.id));
                    vec![
                        format!("+CMGR: 0,,{}", message.tpdu_len),
                        message.pdu_hex.clone(),
                    ]
                }
                _ => vec![],
            },
            _ if upper.starts_with("AT+CMGD=") => {
                let arguments = &upper[8..];
                let delete_all = arguments
                    .split_once(',')
                    .and_then(|(_, flag)| flag.trim().parse::<u32>().ok())
                    .is_some_and(|flag| flag > 0);
                if (delete_all || parse_index(arguments) == Some(MESSAGE_INDEX))
                    && let Some(message) = self.inbox.take()
                {
                    output.events.push(Event::Deleted(message.id));
                }
                vec![]
            }
            _ if upper.starts_with("AT+CMGS=") => {
                self.mode = Mode::SmsEntry;
                self.buffer.clear();
                output.bytes.extend(b"\r\n> ");
                return;
            }
            _ => {
                debug!("answering OK to {command:?}");
                vec![]
            }
        };
        if !info.is_empty() {
            output
                .bytes
                .extend(format!("\r\n{}\r\n", info.join("\r\n")).as_bytes());
        }
        output.bytes.extend(b"\r\nOK\r\n");
    }

    fn submit(&mut self, hex: &[u8], output: &mut Output) {
        let pdu = std::str::from_utf8(hex)
            .ok()
            .and_then(|hex| pdu::from_hex(hex.trim()));
        match pdu {
            Some(pdu) => {
                self.message_reference = self.message_reference.wrapping_add(1);
                output.bytes.extend(
                    format!("\r\n+CMGS: {}\r\n\r\nOK\r\n", self.message_reference).as_bytes(),
                );
                output.events.push(Event::Submitted(pdu));
            }
            None => output.bytes.extend(b"\r\n+CMS ERROR: 304\r\n"),
        }
    }
}

fn parse_index(arguments: &str) -> Option<u32> {
    arguments.split(',').next()?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exchange(modem: &mut Modem, input: &str) -> String {
        String::from_utf8(modem.feed(input.as_bytes()).bytes).unwrap()
    }

    fn message() -> InboxMessage {
        InboxMessage {
            id: 7,
            pdu_hex: "0791534874895300040C9153481032547600006290621245822105CF7A3BEC06".into(),
            tpdu_len: 24,
        }
    }

    #[test]
    fn answers_the_initialization_sequence_of_the_controller() {
        let mut modem = Modem::new("+358447983500");
        let expected = [
            ("AT\r", "\r\nOK\r\n"),
            ("\x1a\r", ""),
            ("AT&F\r", "\r\nOK\r\n"),
            ("ATE0\r", "\r\nOK\r\n"),
            ("AT+CPIN?\r", "\r\n+CPIN: READY\r\n\r\nOK\r\n"),
            ("AT+CSCS=\"GSM\"\r", "\r\nOK\r\n"),
            ("AT+CSMS=1\r", "\r\n+CSMS: 1,1,1\r\n\r\nOK\r\n"),
            ("AT+CMGF=0\r", "\r\nOK\r\n"),
            ("AT+CNMI=0,0,0,0\r", "\r\nOK\r\n"),
            (
                "AT+CPMS=?\r",
                "\r\n+CPMS: (\"SM\"),(\"SM\"),(\"SM\")\r\n\r\nOK\r\n",
            ),
            (
                "AT+CPMS=\"SM\",\"SM\",\"SM\"\r",
                "\r\n+CPMS: 0,20,0,20,0,20\r\n\r\nOK\r\n",
            ),
            (
                "AT+CSCA?\r",
                "\r\n+CSCA: \"+358447983500\",145\r\n\r\nOK\r\n",
            ),
            ("AT+CSCA=\"+358447983500\",145\r", "\r\nOK\r\n"),
            ("AT+CMGL=4\r", "\r\nOK\r\n"),
        ];
        for (input, output) in expected {
            assert_eq!(exchange(&mut modem, input), output, "reply to {input:?}");
        }
    }

    #[test]
    fn lists_and_deletes_a_message() {
        let mut modem = Modem::new("");
        modem.put(message());
        assert_eq!(
            exchange(&mut modem, "AT+CPMS=\"SM\",\"SM\",\"SM\"\r"),
            "\r\n+CPMS: 1,20,1,20,1,20\r\n\r\nOK\r\n"
        );
        let output = modem.feed(b"AT+CMGL=4\r");
        assert_eq!(
            String::from_utf8(output.bytes).unwrap(),
            format!("\r\n+CMGL: 1,0,,24\r\n{}\r\n\r\nOK\r\n", message().pdu_hex)
        );
        assert_eq!(output.events, vec![Event::Polled, Event::Listed(7)]);
        let output = modem.feed(b"AT+CMGD=1\r");
        assert_eq!(output.events, vec![Event::Deleted(7)]);
        assert_eq!(exchange(&mut modem, "AT+CMGL=4\r"), "\r\nOK\r\n");
    }

    #[test]
    fn withdraws_only_the_same_message() {
        let mut modem = Modem::new("");
        modem.put(message());
        assert!(!modem.withdraw(8));
        assert!(modem.withdraw(7));
        assert!(!modem.withdraw(7));
    }

    #[test]
    fn receives_an_sms_from_the_controller() {
        let mut modem = Modem::new("");
        assert_eq!(exchange(&mut modem, "AT+CMGS=42\r"), "\r\n> ");
        let output = modem.feed(b"0011\x1a");
        assert_eq!(
            String::from_utf8(output.bytes).unwrap(),
            "\r\n+CMGS: 1\r\n\r\nOK\r\n"
        );
        assert_eq!(output.events, vec![Event::Submitted(vec![0x00, 0x11])]);
        // The controller sends CR after Ctrl-Z. It must not produce a reply.
        assert_eq!(exchange(&mut modem, "\r"), "");
        assert_eq!(exchange(&mut modem, "AT\r"), "\r\nOK\r\n");
    }

    #[test]
    fn handles_commands_split_over_several_reads() {
        let mut modem = Modem::new("");
        assert_eq!(exchange(&mut modem, "AT+CP"), "");
        assert_eq!(
            exchange(&mut modem, "IN?\rAT\r"),
            "\r\n+CPIN: READY\r\n\r\nOK\r\n\r\nOK\r\n"
        );
    }

    #[test]
    fn cancels_an_sms_entry_with_escape() {
        let mut modem = Modem::new("");
        exchange(&mut modem, "AT+CMGS=10\r");
        let output = modem.feed(b"0011\x1b");
        assert!(output.events.is_empty());
        assert_eq!(exchange(&mut modem, "AT\r"), "\r\nOK\r\n");
    }
}
