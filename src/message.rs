//! The controller's message grammar: `TITLE: segment/ segment/ ...`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Segment {
    Field { label: String, value: String },
    Option { text: String, selected: bool },
    Text { text: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reply {
    pub raw: String,
    pub title: String,
    pub segments: Vec<Segment>,
}

pub fn parse(raw: &str) -> Reply {
    let Some((title, body)) = raw.split_once(':') else {
        return Reply {
            raw: raw.to_string(),
            title: raw.trim().to_string(),
            segments: Vec::new(),
        };
    };
    let parts: Vec<&str> = body
        .split('/')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    // Only a message with a selected option is an option list. In other messages, the parts
    // without "=" are plain text, for example "L1 Normaalilämpö" in the OUMAN reply.
    let is_option_list = parts.iter().any(|p| p.starts_with('*'));
    let segments = parts
        .into_iter()
        .map(|part| {
            if let Some((label, value)) = part.split_once('=') {
                Segment::Field {
                    label: label.trim().to_string(),
                    value: value.trim().to_string(),
                }
            } else if let Some(option) = part.strip_prefix('*') {
                Segment::Option {
                    text: option.trim().to_string(),
                    selected: true,
                }
            } else if is_option_list && !part.ends_with(':') {
                Segment::Option {
                    text: part.to_string(),
                    selected: false,
                }
            } else {
                Segment::Text {
                    text: part.to_string(),
                }
            }
        })
        .collect();
    Reply {
        raw: raw.to_string(),
        title: title.trim().to_string(),
        segments,
    }
}

pub fn build_set(keyword: &str, fields: &[(String, String)]) -> Result<String, String> {
    check_keyword(keyword)?;
    if fields.is_empty() {
        return Err("give at least one field".into());
    }
    let mut segments = Vec::new();
    for (label, value) in fields {
        let (label, value) = (label.trim(), value.trim());
        if label.is_empty() || label.contains(['=', '/', ':']) {
            return Err(format!("{label:?} is not a valid field label"));
        }
        if value.is_empty() || value.contains(['=', '/']) {
            return Err(format!("{value:?} is not a valid value for {label:?}"));
        }
        segments.push(format!("{label}={value}"));
    }
    Ok(format!(
        "{}: {}",
        keyword.trim().to_uppercase(),
        segments.join("/ ")
    ))
}

pub fn build_select(keyword: &str, option: &str) -> Result<String, String> {
    check_keyword(keyword)?;
    let option = option.trim();
    if option.is_empty() || option.contains(['=', '/', '*']) {
        return Err(format!("{option:?} is not a valid option"));
    }
    Ok(format!("{}: *{option}", keyword.trim().to_uppercase()))
}

fn check_keyword(keyword: &str) -> Result<(), String> {
    let keyword = keyword.trim();
    if keyword.is_empty() || keyword.contains([':', '/', '=', '*']) {
        return Err(format!("{keyword:?} is not a valid keyword"));
    }
    Ok(())
}

/// Returns a description of each requested field that the reply does not confirm.
pub fn check_set(reply: &Reply, fields: &[(String, String)]) -> Vec<String> {
    fields
        .iter()
        .filter_map(|(label, requested)| {
            let found = reply.segments.iter().find_map(|segment| match segment {
                Segment::Field { label: l, value } if same_text(l, label) => Some(value),
                _ => None,
            });
            match found {
                None => Some(format!("the reply has no field {:?}", label.trim())),
                Some(value) if !same_value(value, requested) => Some(format!(
                    "{:?} is {value:?} in the reply, requested {:?}",
                    label.trim(),
                    requested.trim()
                )),
                Some(_) => None,
            }
        })
        .collect()
}

/// Returns a description of the problem if the reply does not show `option` as selected.
pub fn check_select(reply: &Reply, option: &str) -> Option<String> {
    let selected = reply.segments.iter().find_map(|segment| match segment {
        Segment::Option {
            text,
            selected: true,
        } => Some(text),
        _ => None,
    });
    match selected {
        None => Some("the reply has no selected option".into()),
        Some(text) if !same_text(text, option) => Some(format!(
            "the selected option is {text:?} in the reply, requested {:?}",
            option.trim()
        )),
        Some(_) => None,
    }
}

fn same_text(a: &str, b: &str) -> bool {
    a.trim().to_lowercase() == b.trim().to_lowercase()
}

fn same_value(a: &str, b: &str) -> bool {
    match (a.trim().parse::<f64>(), b.trim().parse::<f64>()) {
        (Ok(x), Ok(y)) => (x - y).abs() < 1e-9,
        _ => same_text(a, b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(label: &str, value: &str) -> Segment {
        Segment::Field {
            label: label.into(),
            value: value.into(),
        }
    }

    fn option(text: &str, selected: bool) -> Segment {
        Segment::Option {
            text: text.into(),
            selected,
        }
    }

    fn text(text: &str) -> Segment {
        Segment::Text { text: text.into() }
    }

    fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|&(l, v)| (l.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn parses_a_status_reply() {
        let reply = parse(
            "OUMAN: Ulkolämpötila=11.5/ L1 Menoveden lämpötila=23.8/ \
             L1 Laskennall. menoveden asetusarvo=25.0/ L1 Normaalilämpö",
        );
        assert_eq!(reply.title, "OUMAN");
        assert_eq!(
            reply.segments,
            vec![
                field("Ulkolämpötila", "11.5"),
                field("L1 Menoveden lämpötila", "23.8"),
                field("L1 Laskennall. menoveden asetusarvo", "25.0"),
                text("L1 Normaalilämpö"),
            ]
        );
    }

    #[test]
    fn parses_an_option_list() {
        let reply = parse(
            "L1 OHJAUSTAVAT: *Automaatti/ PAKKO-OHJAUS: / Jatkuva normaalilämpö/ Lämmönpudotus/ \
             Suuri lämmönpudotus/ Käsiajo, sähköinen (asento 20% )/ Alasajo",
        );
        assert_eq!(reply.title, "L1 OHJAUSTAVAT");
        assert_eq!(
            reply.segments,
            vec![
                option("Automaatti", true),
                text("PAKKO-OHJAUS:"),
                option("Jatkuva normaalilämpö", false),
                option("Lämmönpudotus", false),
                option("Suuri lämmönpudotus", false),
                option("Käsiajo, sähköinen (asento 20% )", false),
                option("Alasajo", false),
            ]
        );
    }

    #[test]
    fn parses_a_list_with_a_trailing_separator_and_a_time_stamp() {
        let reply = parse("AVAINSANAT: Mittaukset/ L1 asetusarvot/ Hälytykset/ Tyyppitiedot/");
        assert_eq!(reply.segments.len(), 4);
        assert!(
            reply
                .segments
                .iter()
                .all(|s| matches!(s, Segment::Text { .. }))
        );

        let reply = parse("L1 MENOVESI-INFO: L1 Menoveden lämpötila=23.8/ LA 26.9.2026 22:11");
        assert_eq!(reply.segments[1], text("LA 26.9.2026 22:11"));
    }

    #[test]
    fn parses_a_message_without_a_title() {
        let reply = parse("KOTONA");
        assert_eq!(reply.title, "KOTONA");
        assert!(reply.segments.is_empty());
    }

    #[test]
    fn builds_partial_messages() {
        assert_eq!(
            build_set(
                "L1 asetusarvot",
                &pairs(&[("Menoveden minimiraja", "26.0")])
            )
            .unwrap(),
            "L1 ASETUSARVOT: Menoveden minimiraja=26.0"
        );
        assert_eq!(
            build_set("l2 asetusarvot", &pairs(&[("a", "1"), ("b", "2")])).unwrap(),
            "L2 ASETUSARVOT: a=1/ b=2"
        );
        assert_eq!(
            build_select("L1 ohjaustavat", "Jatkuva normaalilämpö").unwrap(),
            "L1 OHJAUSTAVAT: *Jatkuva normaalilämpö"
        );
    }

    #[test]
    fn refuses_text_that_would_break_the_grammar() {
        assert!(build_set("L1 asetusarvot", &[]).is_err());
        assert!(build_set("L1 asetusarvot", &pairs(&[("a/b", "1")])).is_err());
        assert!(build_set("L1 asetusarvot", &pairs(&[("a", "1/2")])).is_err());
        assert!(build_set("L1: x", &pairs(&[("a", "1")])).is_err());
        assert!(build_select("L1 ohjaustavat", "*Automaatti").is_err());
        assert!(build_select("", "Automaatti").is_err());
    }

    #[test]
    fn checks_a_set_reply() {
        let reply = parse("L1 ASETUSARVOT: Menoveden minimiraja=26.0");
        assert!(check_set(&reply, &pairs(&[("menoveden minimiraja", "26")])).is_empty());
        assert_eq!(
            check_set(&reply, &pairs(&[("Menoveden minimiraja", "27")])),
            vec![r#""Menoveden minimiraja" is "26.0" in the reply, requested "27""#]
        );
        assert_eq!(
            check_set(&reply, &pairs(&[("Menoveden maksimiraja", "43")])),
            vec![r#"the reply has no field "Menoveden maksimiraja""#]
        );
    }

    #[test]
    fn checks_a_select_reply() {
        let reply = parse("L1 OHJAUSTAVAT: Automaatti/ *Jatkuva normaalilämpö/ Alasajo");
        assert_eq!(check_select(&reply, "jatkuva normaalilämpö"), None);
        assert_eq!(
            check_select(&reply, "Automaatti"),
            Some(r#"the selected option is "Jatkuva normaalilämpö" in the reply, requested "Automaatti""#.into())
        );
        assert!(check_select(&parse("OUMAN: a=1"), "Automaatti").is_some());
    }
}
