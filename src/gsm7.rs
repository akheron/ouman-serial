//! The GSM 03.38 default 7-bit alphabet, with its extension table, and septet packing.

use std::sync::LazyLock;

const BASIC: &str = "@£$¥èéùìòÇ\nØø\rÅåΔ_ΦΓΛΩΠΨΣΘΞ\u{1b}ÆæßÉ !\"#¤%&'()*+,-./0123456789:;<=>?\
                     ¡ABCDEFGHIJKLMNOPQRSTUVWXYZÄÖÑÜ§¿abcdefghijklmnopqrstuvwxyzäöñüà";

const EXTENSION: [(u8, char); 10] = [
    (0x0A, '\u{0c}'),
    (0x14, '^'),
    (0x28, '{'),
    (0x29, '}'),
    (0x2F, '\\'),
    (0x3C, '['),
    (0x3D, '~'),
    (0x3E, ']'),
    (0x40, '|'),
    (0x65, '€'),
];

const ESCAPE: u8 = 0x1B;

static BASIC_TABLE: LazyLock<Vec<char>> = LazyLock::new(|| {
    let table: Vec<char> = BASIC.chars().collect();
    assert_eq!(table.len(), 128);
    table
});

/// Converts text to septets. Returns the first character that the alphabet does not have.
pub fn encode(text: &str) -> Result<Vec<u8>, char> {
    let mut septets = Vec::with_capacity(text.len());
    for ch in text.chars() {
        if let Some(index) = BASIC_TABLE.iter().position(|&c| c == ch && c != '\u{1b}') {
            septets.push(index as u8);
        } else if let Some(&(code, _)) = EXTENSION.iter().find(|&&(_, c)| c == ch) {
            septets.extend([ESCAPE, code]);
        } else {
            return Err(ch);
        }
    }
    Ok(septets)
}

pub fn decode(septets: &[u8]) -> String {
    let mut text = String::with_capacity(septets.len());
    let mut escaped = false;
    for &septet in septets {
        let septet = septet & 0x7F;
        if escaped {
            let ch = EXTENSION
                .iter()
                .find(|&&(code, _)| code == septet)
                .map_or('?', |&(_, c)| c);
            text.push(ch);
            escaped = false;
        } else if septet == ESCAPE {
            escaped = true;
        } else {
            text.push(BASIC_TABLE[septet as usize]);
        }
    }
    text
}

pub fn pack(septets: &[u8]) -> Vec<u8> {
    let mut packed = vec![0u8; (septets.len() * 7).div_ceil(8)];
    for (i, &septet) in septets.iter().enumerate() {
        let bit = i * 7;
        let value = u16::from(septet & 0x7F) << (bit % 8);
        packed[bit / 8] |= value as u8;
        if let Some(next) = packed.get_mut(bit / 8 + 1) {
            *next |= (value >> 8) as u8;
        }
    }
    packed
}

/// Reads `count` septets that start `bit_offset` bits into `data`. Missing bits read as zero.
pub fn unpack(data: &[u8], count: usize, bit_offset: usize) -> Vec<u8> {
    (0..count)
        .map(|i| {
            let bit = bit_offset + i * 7;
            let low = u16::from(data.get(bit / 8).copied().unwrap_or(0));
            let high = u16::from(data.get(bit / 8 + 1).copied().unwrap_or(0));
            (((high << 8 | low) >> (bit % 8)) & 0x7F) as u8
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packs_the_reference_example() {
        let packed = pack(&encode("hellohello").unwrap());
        assert_eq!(hex(&packed), "E8329BFD4697D9EC37");
        assert_eq!(decode(&unpack(&packed, 10, 0)), "hellohello");
    }

    #[test]
    fn round_trips_finnish_letters_and_extension_characters() {
        let text = "Lämmönpudotus Åland ÖÄ [5€] ~^";
        let septets = encode(text).unwrap();
        assert_eq!(decode(&unpack(&pack(&septets), septets.len(), 0)), text);
    }

    #[test]
    fn extension_characters_take_two_septets() {
        assert_eq!(encode("€").unwrap(), vec![0x1B, 0x65]);
    }

    #[test]
    fn rejects_characters_outside_the_alphabet() {
        assert_eq!(encode("ok ő"), Err('ő'));
        assert_eq!(encode("\u{1b}"), Err('\u{1b}'));
    }

    #[test]
    fn unpacks_from_a_bit_offset() {
        // Behind a 6-octet user data header and one fill bit, the text starts at bit 49.
        let mut septets = vec![0u8; 7];
        septets.extend(encode("abc").unwrap());
        assert_eq!(decode(&unpack(&pack(&septets), 3, 49)), "abc");
    }

    fn hex(data: &[u8]) -> String {
        data.iter().map(|b| format!("{b:02X}")).collect()
    }
}
