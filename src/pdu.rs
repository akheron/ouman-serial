//! SMS PDUs (GSM 03.40): SMS-DELIVER to the controller, and SMS-SUBMIT from the controller.

use std::fmt;

use chrono::{DateTime, Datelike, FixedOffset, Timelike};

use crate::gsm7;

pub const MAX_SEPTETS: usize = 160;

#[derive(Debug, PartialEq)]
pub struct PduError(pub String);

impl fmt::Display for PduError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PduError {}

fn error<T>(message: impl Into<String>) -> Result<T, PduError> {
    Err(PduError(message.into()))
}

/// An SMS-DELIVER PDU in the form that `AT+CMGL` lists it.
#[derive(Debug, PartialEq)]
pub struct Deliver {
    /// The SMSC address and the TPDU, in upper-case hex.
    pub hex: String,
    /// The TPDU length in octets, without the SMSC address.
    pub tpdu_len: usize,
}

#[derive(Debug, PartialEq)]
pub struct Concat {
    pub reference: u16,
    pub total: u8,
    pub part: u8,
}

#[derive(Debug, PartialEq)]
pub struct Submit {
    pub destination: String,
    pub concat: Option<Concat>,
    pub text: String,
}

/// Returns the number of GSM 7-bit septets that `text` needs in one SMS.
pub fn septet_count(text: &str) -> Result<usize, PduError> {
    let septets = gsm7::encode(text).or_else(|ch| {
        error(format!(
            "the character {ch:?} is not in the GSM 7-bit alphabet"
        ))
    })?;
    if septets.len() > MAX_SEPTETS {
        return error(format!(
            "the message is {} GSM 7-bit characters long, the maximum is {MAX_SEPTETS}",
            septets.len()
        ));
    }
    Ok(septets.len())
}

pub fn encode_deliver(
    smsc: &str,
    sender: &str,
    text: &str,
    time: DateTime<FixedOffset>,
) -> Result<Deliver, PduError> {
    septet_count(text)?;
    let septets = gsm7::encode(text).expect("septet_count checked the alphabet");
    let mut tpdu = vec![0x04];
    tpdu.extend(encode_address(sender)?);
    tpdu.extend([0x00, 0x00]);
    tpdu.extend(encode_timestamp(time));
    tpdu.push(septets.len() as u8);
    tpdu.extend(gsm7::pack(&septets));
    let mut pdu = encode_smsc(smsc)?;
    pdu.extend(&tpdu);
    Ok(Deliver {
        hex: to_hex(&pdu),
        tpdu_len: tpdu.len(),
    })
}

pub fn decode_submit(pdu: &[u8]) -> Result<Submit, PduError> {
    let mut reader = Reader::new(pdu);
    let smsc_len = reader.byte()?;
    reader.take(smsc_len.into())?;
    let first = reader.byte()?;
    if first & 0x03 != 0x01 {
        return error(format!("not an SMS-SUBMIT: first octet {first:02X}"));
    }
    reader.byte()?; // message reference
    let destination = decode_address(&mut reader)?;
    reader.byte()?; // protocol identifier
    let dcs = reader.byte()?;
    match (first >> 3) & 0x03 {
        0 => {}
        2 => {
            reader.take(1)?;
        }
        _ => {
            reader.take(7)?;
        }
    }
    let (concat, text) = decode_user_data(&mut reader, dcs, first & 0x40 != 0)?;
    Ok(Submit {
        destination,
        concat,
        text,
    })
}

pub fn to_hex(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02X}")).collect()
}

pub fn from_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

fn parse_number(number: &str) -> Result<(u8, &str), PduError> {
    let (type_of_address, digits) = match number.strip_prefix('+') {
        Some(digits) => (0x91, digits),
        None => (0x81, number),
    };
    if digits.is_empty() || digits.len() > 20 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return error(format!("{number:?} is not a phone number"));
    }
    Ok((type_of_address, digits))
}

fn semi_octets(digits: &str) -> Vec<u8> {
    digits
        .as_bytes()
        .chunks(2)
        .map(|pair| {
            let low = pair[0] - b'0';
            let high = pair.get(1).map_or(0x0F, |digit| digit - b'0');
            high << 4 | low
        })
        .collect()
}

fn encode_address(number: &str) -> Result<Vec<u8>, PduError> {
    let (type_of_address, digits) = parse_number(number)?;
    let mut address = vec![digits.len() as u8, type_of_address];
    address.extend(semi_octets(digits));
    Ok(address)
}

fn encode_smsc(number: &str) -> Result<Vec<u8>, PduError> {
    if number.is_empty() {
        return Ok(vec![0x00]);
    }
    let (type_of_address, digits) = parse_number(number)?;
    let body = semi_octets(digits);
    let mut smsc = vec![body.len() as u8 + 1, type_of_address];
    smsc.extend(body);
    Ok(smsc)
}

fn encode_timestamp(time: DateTime<FixedOffset>) -> [u8; 7] {
    let bcd = |value: u32| {
        let value = value % 100;
        (((value % 10) << 4) | (value / 10)) as u8
    };
    let quarters = time.offset().local_minus_utc() / 900;
    let mut zone = bcd(quarters.unsigned_abs());
    if quarters < 0 {
        zone |= 0x08;
    }
    [
        bcd(time.year().unsigned_abs()),
        bcd(time.month()),
        bcd(time.day()),
        bcd(time.hour()),
        bcd(time.minute()),
        bcd(time.second()),
        zone,
    ]
}

fn decode_address(reader: &mut Reader) -> Result<String, PduError> {
    let length = usize::from(reader.byte()?);
    let type_of_address = reader.byte()?;
    let raw = reader.take(length.div_ceil(2))?;
    if type_of_address & 0x70 == 0x50 {
        return Ok(gsm7::decode(&gsm7::unpack(raw, length * 4 / 7, 0)));
    }
    let mut number = String::new();
    if type_of_address & 0x70 == 0x10 {
        number.push('+');
    }
    for nibble in raw.iter().flat_map(|b| [b & 0x0F, b >> 4]).take(length) {
        number.push(char::from(b"0123456789*#abc?"[usize::from(nibble)]));
    }
    Ok(number)
}

enum Alphabet {
    Gsm7,
    EightBit,
    Ucs2,
}

fn alphabet(dcs: u8) -> Alphabet {
    match dcs >> 4 {
        0x0..=0x3 => match (dcs >> 2) & 0x03 {
            1 => Alphabet::EightBit,
            2 => Alphabet::Ucs2,
            _ => Alphabet::Gsm7,
        },
        0xE => Alphabet::Ucs2,
        0xF if dcs & 0x04 != 0 => Alphabet::EightBit,
        _ => Alphabet::Gsm7,
    }
}

fn decode_user_data(
    reader: &mut Reader,
    dcs: u8,
    has_header: bool,
) -> Result<(Option<Concat>, String), PduError> {
    let length = usize::from(reader.byte()?);
    let data = reader.rest();
    let header_len = if has_header {
        usize::from(*data.first().ok_or(PduError("no user data header".into()))?) + 1
    } else {
        0
    };
    let concat = match data.get(1..header_len) {
        Some(elements) if has_header => find_concat(elements),
        None if has_header => return error("the user data header is longer than the data"),
        _ => None,
    };
    let text = match alphabet(dcs) {
        Alphabet::Gsm7 => {
            if data.len() < (length * 7).div_ceil(8) {
                return error("the user data is shorter than its length");
            }
            let header_septets = (header_len * 8).div_ceil(7);
            let count = length.checked_sub(header_septets).ok_or(PduError(
                "the user data header is longer than the user data".into(),
            ))?;
            gsm7::decode(&gsm7::unpack(data, count, header_septets * 7))
        }
        Alphabet::EightBit => octets(data, header_len, length)?
            .iter()
            .map(|&b| char::from(b))
            .collect(),
        Alphabet::Ucs2 => {
            let units = octets(data, header_len, length)?
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| u16::from_be_bytes([pair[0], pair[1]]));
            char::decode_utf16(units)
                .map(|c| c.unwrap_or('\u{fffd}'))
                .collect()
        }
    };
    Ok((concat, text))
}

fn octets(data: &[u8], start: usize, end: usize) -> Result<&[u8], PduError> {
    data.get(start..end)
        .ok_or(PduError("the user data is shorter than its length".into()))
}

fn find_concat(mut elements: &[u8]) -> Option<Concat> {
    while let [id, length, rest @ ..] = elements {
        let (value, next) = rest.split_at_checked(usize::from(*length))?;
        match (id, value) {
            (0x00, &[reference, total, part]) => {
                return Some(Concat {
                    reference: reference.into(),
                    total,
                    part,
                });
            }
            (0x08, &[high, low, total, part]) => {
                return Some(Concat {
                    reference: u16::from_be_bytes([high, low]),
                    total,
                    part,
                });
            }
            _ => elements = next,
        }
    }
    None
}

struct Reader<'a> {
    data: &'a [u8],
    position: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, position: 0 }
    }

    fn byte(&mut self) -> Result<u8, PduError> {
        Ok(self.take(1)?[0])
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], PduError> {
        let slice = self
            .data
            .get(self.position..self.position + count)
            .ok_or(PduError("the PDU ends too early".into()))?;
        self.position += count;
        Ok(slice)
    }

    fn rest(&mut self) -> &'a [u8] {
        let rest = &self.data[self.position..];
        self.position = self.data.len();
        rest
    }
}

/// The controller's side of the PDU exchange, for the simulated controller in the tests.
#[cfg(test)]
pub mod controller {
    use super::*;

    pub fn encode_submit(destination: &str, text: &str, concat: Option<(u8, u8, u8)>) -> Vec<u8> {
        let text_septets = gsm7::encode(text).unwrap();
        let first = if concat.is_some() { 0x51 } else { 0x11 };
        let mut pdu = vec![0x00, first, 0x00];
        pdu.extend(encode_address(destination).unwrap());
        pdu.extend([0x00, 0x00, 0xAA]);
        match concat {
            None => {
                pdu.push(text_septets.len() as u8);
                pdu.extend(gsm7::pack(&text_septets));
            }
            Some((reference, total, part)) => {
                // Seven zero septets fill the six header octets and the one fill bit.
                let mut septets = vec![0u8; 7];
                septets.extend(&text_septets);
                let mut data = gsm7::pack(&septets);
                data[..6].copy_from_slice(&[0x05, 0x00, 0x03, reference, total, part]);
                pdu.push(septets.len() as u8);
                pdu.extend(data);
            }
        }
        pdu
    }

    /// Returns the sender and the text of an SMS-DELIVER PDU with an SMSC address.
    pub fn decode_deliver(pdu: &[u8]) -> (String, String) {
        let mut reader = Reader::new(pdu);
        let smsc_len = reader.byte().unwrap();
        reader.take(smsc_len.into()).unwrap();
        assert_eq!(reader.byte().unwrap() & 0x03, 0x00, "not an SMS-DELIVER");
        let sender = decode_address(&mut reader).unwrap();
        reader.take(1).unwrap(); // protocol identifier
        let dcs = reader.byte().unwrap();
        reader.take(7).unwrap(); // time stamp
        let (_, text) = decode_user_data(&mut reader, dcs, false).unwrap();
        (sender, text)
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::controller::*;
    use super::*;

    /// Captured from the real controller: the reply to "Ouman".
    const OUMAN_REPLY: &str = "0011000C915348103254760000AA72CF6A33E8D481AAECF59BBD6FC3F9F4343BDC8BD55CB11788190335CBEEB7BD4C2EBB41EC7D1BCEA7A7D9E19E8CE682BD40CC1888199EAFCB6E7798CD7681DA65F7DB5E2697DDA0F0BC4CAFCFC372FBBB27ABB9602F10330672BEE5ED70989D66EFDB703E";

    /// Captured from the real controller: the reply to a partial write.
    const PARTIAL_WRITE_REPLY: &str = "0011000C915348103254760000AA29CC1828382D52ABD3A0D4FAA4EA40CDB2FB6D2F93CB6E503BED4EB7D3F2B03ADC93D95C30";

    #[test]
    fn encodes_the_deliver_pdu_that_the_controller_accepted() {
        let time = FixedOffset::east_opt(3 * 3600)
            .unwrap()
            .with_ymd_and_hms(2026, 9, 26, 21, 54, 28)
            .unwrap();
        let deliver = encode_deliver("+358447983500", "+358401234567", "Ouman", time).unwrap();
        assert_eq!(
            deliver.hex,
            "0791534874895300040C9153481032547600006290621245822105CF7A3BEC06"
        );
        assert_eq!(deliver.tpdu_len, 24);
    }

    #[test]
    fn encodes_a_negative_time_zone() {
        let time = FixedOffset::west_opt(5 * 3600)
            .unwrap()
            .with_ymd_and_hms(2026, 1, 2, 3, 4, 5)
            .unwrap();
        assert_eq!(encode_timestamp(time)[6], 0x0A);
    }

    #[test]
    fn decodes_real_controller_replies() {
        let reply = decode_submit(&from_hex(OUMAN_REPLY).unwrap()).unwrap();
        assert_eq!(reply.destination, "+358401234567");
        assert_eq!(reply.concat, None);
        assert_eq!(
            reply.text,
            "OUMAN: Ulkolämpötila=15.1/ L1 Menoveden lämpötila=24.0/ \
             L1 Laskennall. menoveden asetusarvo=25.0/ L1 Normaalilämpö"
        );
        let reply = decode_submit(&from_hex(PARTIAL_WRITE_REPLY).unwrap()).unwrap();
        assert_eq!(reply.text, "L1 ASETUSARVOT: Menoveden minimiraja=26.0");
    }

    #[test]
    fn decodes_a_concatenated_part() {
        let pdu = encode_submit("+35800000042", "Menovesi-info part 1", Some((7, 2, 1)));
        let part = decode_submit(&pdu).unwrap();
        assert_eq!(part.destination, "+35800000042");
        assert_eq!(
            part.concat,
            Some(Concat {
                reference: 7,
                total: 2,
                part: 1
            })
        );
        assert_eq!(part.text, "Menovesi-info part 1");
    }

    #[test]
    fn round_trips_deliver_through_the_controller_side() {
        let time = FixedOffset::east_opt(0)
            .unwrap()
            .with_ymd_and_hms(2026, 9, 27, 12, 0, 0)
            .unwrap();
        let deliver = encode_deliver("", "+35800001234", "L1 asetusarvot", time).unwrap();
        let (sender, text) = decode_deliver(&from_hex(&deliver.hex).unwrap());
        assert_eq!(sender, "+35800001234");
        assert_eq!(text, "L1 asetusarvot");
    }

    #[test]
    fn rejects_long_and_unsupported_text() {
        let time = FixedOffset::east_opt(0)
            .unwrap()
            .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
            .unwrap();
        assert!(encode_deliver("", "+358", &"x".repeat(161), time).is_err());
        assert!(encode_deliver("", "+358", &"x".repeat(160), time).is_ok());
        assert!(encode_deliver("", "+358", "€".repeat(81).as_str(), time).is_err());
        assert!(encode_deliver("", "+358", "ő", time).is_err());
        assert!(encode_deliver("", "12ab", "x", time).is_err());
    }

    #[test]
    fn rejects_truncated_pdus() {
        let pdu = from_hex(OUMAN_REPLY).unwrap();
        assert!(decode_submit(&pdu[..20]).is_err());
        assert!(decode_submit(&[]).is_err());
    }

    #[test]
    fn hex_round_trip() {
        assert_eq!(from_hex("00AAff"), Some(vec![0x00, 0xAA, 0xFF]));
        assert_eq!(from_hex("0"), None);
        assert_eq!(from_hex("zz"), None);
        assert_eq!(to_hex(&[0x0A, 0xB0]), "0AB0");
    }
}
