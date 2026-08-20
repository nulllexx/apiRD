//! A reader for Minecraft's stringified NBT, the format `/data get` replies in.
//!
//! This exists so the live inventory view can reuse the same walk as the saved
//! one. RCON hands back text like
//!
//! ```text
//! Steve has the following entity data: [{Slot: 0b, id: "minecraft:stone", count: 64}]
//! ```
//!
//! and the parser in `inventory.rs` already knows how to read a
//! [`fastnbt::Value`], so turning that text into a `Value` means there is one
//! item-reading implementation rather than two that can disagree.
//!
//! `fastsnbt` is the obvious crate for this and does not work here: its
//! deserializer is not self-describing, so `from_str::<Value>` fails on
//! anything but an empty compound. Deserializing into typed structs instead
//! would give up exactly the version-tolerance that `inventory.rs` is built
//! around, so the format gets a small hand-written reader.
//!
//! Only the parts vanilla actually emits are supported: compounds, lists, the
//! three typed arrays, quoted and bare strings, and numbers with their type
//! suffix. That is the whole of what `/data get` produces.

use std::collections::HashMap;

use fastnbt::{ByteArray, IntArray, LongArray, Value};

/// Nesting limit. Vanilla data is a handful of levels deep; this stops a
/// malformed or hostile reply from recursing the parser into a stack overflow,
/// which in a request handler would take the whole server down.
const MAX_DEPTH: usize = 64;

#[derive(Debug, PartialEq)]
pub struct SnbtError {
    pub message: String,
    pub position: usize,
}

impl std::fmt::Display for SnbtError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} at byte {}", self.message, self.position)
    }
}

/// Strip the prose `/data get` wraps its answer in.
///
/// The reply is `<name> has the following entity data: <snbt>`, and the server
/// says something else entirely — "No entity was found" — when the player is
/// not online. Splitting on the first `": "` separates the two cases without
/// having to match on wording that a plugin or a translation could change: a
/// name cannot contain a colon, so the first one is always the separator.
///
/// Returns `None` when there is no payload, which the caller reads as "the
/// server would not answer this" rather than as a parse failure.
pub fn strip_reply_prefix(raw: &str) -> Option<&str> {
    let body = raw.split_once(": ").map(|(_, rest)| rest).unwrap_or(raw);
    let body = body.trim();
    (!body.is_empty()).then_some(body)
}

/// Parse one SNBT value.
pub fn parse(input: &str) -> Result<Value, SnbtError> {
    let mut parser = Parser {
        bytes: input.as_bytes(),
        position: 0,
    };

    parser.skip_whitespace();
    let value = parser.value(0)?;
    parser.skip_whitespace();

    if parser.position != parser.bytes.len() {
        return Err(parser.error("trailing characters after the value"));
    }

    Ok(value)
}

struct Parser<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Parser<'a> {
    fn error(&self, message: &str) -> SnbtError {
        SnbtError {
            message: message.to_string(),
            position: self.position,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.position).copied()
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.position += 1;
        }
    }

    /// Consume `expected`, or report where it should have been.
    fn expect(&mut self, expected: u8) -> Result<(), SnbtError> {
        if self.peek() == Some(expected) {
            self.position += 1;
            Ok(())
        } else {
            Err(self.error(&format!("expected {:?}", expected as char)))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Value, SnbtError> {
        if depth > MAX_DEPTH {
            return Err(self.error("value is nested too deeply"));
        }

        match self.peek() {
            Some(b'{') => self.compound(depth),
            Some(b'[') => self.list_or_array(depth),
            Some(b'"') | Some(b'\'') => Ok(Value::String(self.quoted_string()?)),
            Some(_) => self.bare_value(),
            None => Err(self.error("unexpected end of input")),
        }
    }

    fn compound(&mut self, depth: usize) -> Result<Value, SnbtError> {
        self.expect(b'{')?;
        let mut map = HashMap::new();

        loop {
            self.skip_whitespace();
            if self.peek() == Some(b'}') {
                self.position += 1;
                return Ok(Value::Compound(map));
            }

            let key = match self.peek() {
                Some(b'"') | Some(b'\'') => self.quoted_string()?,
                _ => {
                    let bare = self.bare_token();
                    if bare.is_empty() {
                        return Err(self.error("expected a key"));
                    }
                    bare
                }
            };

            self.skip_whitespace();
            self.expect(b':')?;
            self.skip_whitespace();

            let value = self.value(depth + 1)?;
            map.insert(key, value);

            self.skip_whitespace();
            match self.peek() {
                Some(b',') => self.position += 1,
                Some(b'}') => {}
                _ => return Err(self.error("expected ',' or '}'")),
            }
        }
    }

    /// A `[` opens either a plain list or one of the three typed arrays, which
    /// announce themselves with a `B;`, `I;` or `L;` prefix.
    fn list_or_array(&mut self, depth: usize) -> Result<Value, SnbtError> {
        self.expect(b'[')?;

        let array_kind = match (self.peek(), self.bytes.get(self.position + 1)) {
            (Some(kind @ (b'B' | b'I' | b'L')), Some(b';')) => {
                self.position += 2;
                Some(kind)
            }
            _ => None,
        };

        let mut items = Vec::new();
        loop {
            self.skip_whitespace();
            if self.peek() == Some(b']') {
                self.position += 1;
                break;
            }

            items.push(self.value(depth + 1)?);

            self.skip_whitespace();
            match self.peek() {
                Some(b',') => self.position += 1,
                Some(b']') => {}
                _ => return Err(self.error("expected ',' or ']'")),
            }
        }

        let Some(kind) = array_kind else {
            return Ok(Value::List(items));
        };

        // Typed arrays are homogeneous by definition, so anything that is not a
        // number in one is malformed rather than something to keep.
        let numbers: Option<Vec<i64>> = items
            .iter()
            .map(|item| match item {
                Value::Byte(v) => Some(i64::from(*v)),
                Value::Short(v) => Some(i64::from(*v)),
                Value::Int(v) => Some(i64::from(*v)),
                Value::Long(v) => Some(*v),
                _ => None,
            })
            .collect();
        let numbers = numbers.ok_or_else(|| self.error("non-numeric element in a typed array"))?;

        Ok(match kind {
            b'B' => Value::ByteArray(ByteArray::new(
                numbers.iter().map(|v| *v as i8).collect(),
            )),
            b'I' => Value::IntArray(IntArray::new(
                numbers.iter().map(|v| *v as i32).collect(),
            )),
            _ => Value::LongArray(LongArray::new(numbers)),
        })
    }

    fn quoted_string(&mut self) -> Result<String, SnbtError> {
        let quote = self.peek().ok_or_else(|| self.error("expected a quote"))?;
        self.position += 1;

        let mut out = String::new();
        loop {
            let byte = self
                .peek()
                .ok_or_else(|| self.error("unterminated string"))?;
            self.position += 1;

            match byte {
                b'\\' => {
                    let escaped = self
                        .peek()
                        .ok_or_else(|| self.error("unterminated escape"))?;
                    self.position += 1;
                    // SNBT only ever escapes the quote character and the
                    // backslash; anything else is passed through as written.
                    out.push(escaped as char);
                }
                b if b == quote => return Ok(out),
                _ => {
                    // Step back so multi-byte UTF-8 is copied whole rather than
                    // one byte at a time.
                    self.position -= 1;
                    let rest = &self.bytes[self.position..];
                    let width = utf8_width(rest[0]);
                    let slice = rest.get(..width).ok_or_else(|| self.error("bad UTF-8"))?;
                    out.push_str(&String::from_utf8_lossy(slice));
                    self.position += width;
                }
            }
        }
    }

    /// Read the run of characters that can make up a bare key, number or word.
    ///
    /// This is vanilla's unquoted-string set exactly: alphanumerics plus
    /// `_ . + -`. The colon is *not* in it, which is what lets a key end at the
    /// separator — and why the server quotes namespaced ids like
    /// `"minecraft:stone"` when it writes them out.
    fn bare_token(&mut self) -> String {
        let start = self.position;
        while let Some(byte) = self.peek() {
            let ok = byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'+');
            if !ok {
                break;
            }
            self.position += 1;
        }
        String::from_utf8_lossy(&self.bytes[start..self.position]).into_owned()
    }

    /// A bare token is a number if it parses as one, and a string otherwise —
    /// which is exactly how Minecraft itself reads them.
    fn bare_value(&mut self) -> Result<Value, SnbtError> {
        let token = self.bare_token();
        if token.is_empty() {
            return Err(self.error("expected a value"));
        }
        Ok(parse_scalar(&token))
    }
}

fn utf8_width(first: u8) -> usize {
    match first {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

/// Turn a bare token into the value Minecraft would read it as.
///
/// The suffix carries the type: `1b` is a byte, `1.0f` a float, an unsuffixed
/// whole number an int, and an unsuffixed decimal a double. A token that is not
/// a number at all is a string, which is how ids like `minecraft:stone` survive
/// being written without quotes.
fn parse_scalar(token: &str) -> Value {
    if token.eq_ignore_ascii_case("true") {
        return Value::Byte(1);
    }
    if token.eq_ignore_ascii_case("false") {
        return Value::Byte(0);
    }

    let (head, suffix) = token.split_at(token.len() - 1);
    let suffixed = |head: &str| !head.is_empty();

    match suffix {
        "b" | "B" if suffixed(head) => {
            if let Ok(v) = head.parse::<i8>() {
                return Value::Byte(v);
            }
        }
        "s" | "S" if suffixed(head) => {
            if let Ok(v) = head.parse::<i16>() {
                return Value::Short(v);
            }
        }
        "l" | "L" if suffixed(head) => {
            if let Ok(v) = head.parse::<i64>() {
                return Value::Long(v);
            }
        }
        "f" | "F" if suffixed(head) => {
            if let Ok(v) = head.parse::<f32>() {
                return Value::Float(v);
            }
        }
        "d" | "D" if suffixed(head) => {
            if let Ok(v) = head.parse::<f64>() {
                return Value::Double(v);
            }
        }
        _ => {}
    }

    if let Ok(v) = token.parse::<i32>() {
        return Value::Int(v);
    }
    if let Ok(v) = token.parse::<i64>() {
        return Value::Long(v);
    }
    // Only a token that actually looks like a decimal becomes one; `1.21.4`
    // parses as nothing and stays the string it is.
    if token.contains('.') {
        if let Ok(v) = token.parse::<f64>() {
            return Value::Double(v);
        }
    }

    Value::String(token.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compound(value: &Value) -> &HashMap<String, Value> {
        match value {
            Value::Compound(map) => map,
            other => panic!("expected a compound, got {other:?}"),
        }
    }

    fn list(value: &Value) -> &Vec<Value> {
        match value {
            Value::List(items) => items,
            other => panic!("expected a list, got {other:?}"),
        }
    }

    /* ------------------------------------------------------------- scalars */

    #[test]
    fn a_suffix_decides_the_number_type() {
        assert_eq!(parse_scalar("1b"), Value::Byte(1));
        assert_eq!(parse_scalar("-8B"), Value::Byte(-8));
        assert_eq!(parse_scalar("300s"), Value::Short(300));
        assert_eq!(parse_scalar("9000000000L"), Value::Long(9_000_000_000));
        assert_eq!(parse_scalar("1.5f"), Value::Float(1.5));
        assert_eq!(parse_scalar("1.5d"), Value::Double(1.5));
    }

    #[test]
    fn an_unsuffixed_number_is_an_int_or_a_double() {
        assert_eq!(parse_scalar("64"), Value::Int(64));
        assert_eq!(parse_scalar("-1"), Value::Int(-1));
        assert_eq!(parse_scalar("20.0"), Value::Double(20.0));
        // Too big for an int, so it widens rather than overflowing.
        assert_eq!(parse_scalar("3000000000"), Value::Long(3_000_000_000));
    }

    #[test]
    fn booleans_are_bytes() {
        assert_eq!(parse_scalar("true"), Value::Byte(1));
        assert_eq!(parse_scalar("false"), Value::Byte(0));
    }

    #[test]
    fn a_token_that_is_not_a_number_stays_a_string() {
        // Unquoted ids are the reason this matters: `minecraft:stone` ends in
        // `e`, and a naive suffix check must not turn version-like tokens into
        // numbers either.
        assert_eq!(
            parse_scalar("minecraft:stone"),
            Value::String("minecraft:stone".to_string())
        );
        assert_eq!(parse_scalar("1.21.4"), Value::String("1.21.4".to_string()));
        assert_eq!(parse_scalar("b"), Value::String("b".to_string()));
        assert_eq!(parse_scalar("survival"), Value::String("survival".to_string()));
    }

    /* ----------------------------------------------------------- structure */

    #[test]
    fn reads_an_empty_compound_and_list() {
        assert_eq!(parse("{}").unwrap(), Value::Compound(HashMap::new()));
        assert_eq!(parse("[]").unwrap(), Value::List(Vec::new()));
    }

    #[test]
    fn reads_a_flat_compound() {
        let value = parse("{Health: 20.0f, foodLevel: 18, Dimension: \"minecraft:overworld\"}")
            .unwrap();
        let map = compound(&value);

        assert_eq!(map.get("Health"), Some(&Value::Float(20.0)));
        assert_eq!(map.get("foodLevel"), Some(&Value::Int(18)));
        assert_eq!(
            map.get("Dimension"),
            Some(&Value::String("minecraft:overworld".to_string()))
        );
    }

    #[test]
    fn reads_quoted_keys_and_single_quoted_strings() {
        // Component keys are namespaced and therefore quoted; legacy display
        // names are single-quoted JSON.
        let value = parse(r#"{"minecraft:damage": 5, name: 'a "quoted" word'}"#).unwrap();
        let map = compound(&value);

        assert_eq!(map.get("minecraft:damage"), Some(&Value::Int(5)));
        assert_eq!(
            map.get("name"),
            Some(&Value::String("a \"quoted\" word".to_string()))
        );
    }

    #[test]
    fn reads_escapes_inside_a_string() {
        let value = parse(r#"{a: "say \"hi\" \\ bye"}"#).unwrap();
        assert_eq!(
            compound(&value).get("a"),
            Some(&Value::String("say \"hi\" \\ bye".to_string()))
        );
    }

    #[test]
    fn reads_non_ascii_text_whole() {
        // A renamed item can carry anything; copying byte by byte would split
        // the code point and corrupt it.
        let value = parse("{a: \"Épée ⚔\"}").unwrap();
        assert_eq!(
            compound(&value).get("a"),
            Some(&Value::String("Épée ⚔".to_string()))
        );
    }

    #[test]
    fn reads_nested_compounds_and_lists() {
        let value = parse("{a: {b: [1, 2, 3]}}").unwrap();
        let inner = compound(compound(&value).get("a").unwrap());
        assert_eq!(list(inner.get("b").unwrap()).len(), 3);
    }

    #[test]
    fn reads_the_three_typed_arrays() {
        let value = parse("{b: [B; 1b, 2b], i: [I; 1, 2], l: [L; 1l, 2l]}").unwrap();
        let map = compound(&value);

        assert!(matches!(map.get("b"), Some(Value::ByteArray(_))));
        assert!(matches!(map.get("i"), Some(Value::IntArray(_))));
        assert!(matches!(map.get("l"), Some(Value::LongArray(_))));
    }

    #[test]
    fn a_uuid_int_array_survives() {
        // Every modern entity carries one of these, so a parser that chokes on
        // it cannot read anything at all.
        let value = parse("{UUID: [I; -1062731519, 1156087782, -1500213904, 1234567890]}").unwrap();
        match compound(&value).get("UUID") {
            Some(Value::IntArray(array)) => assert_eq!(array.len(), 4),
            other => panic!("expected an int array, got {other:?}"),
        }
    }

    #[test]
    fn tolerates_whitespace_and_newlines() {
        let value = parse("{\n  a : 1 ,\n  b : [ 2 , 3 ]\n}").unwrap();
        assert_eq!(compound(&value).get("a"), Some(&Value::Int(1)));
    }

    /* -------------------------------------------------------------- errors */

    #[test]
    fn rejects_malformed_input() {
        for bad in [
            "{",
            "{a}",
            "{a: }",
            "[1, 2",
            "{a: \"unterminated}",
            "",
            "{a: 1} trailing",
        ] {
            assert!(parse(bad).is_err(), "{bad:?} must not parse");
        }
    }

    #[test]
    fn refuses_input_nested_past_the_depth_limit() {
        // A hostile reply of ten thousand open brackets would otherwise recurse
        // the parser straight into a stack overflow.
        let deep = "[".repeat(MAX_DEPTH + 10);
        let err = parse(&deep).unwrap_err();
        assert!(err.message.contains("deeply"), "got {err}");
    }

    /* ----------------------------------------------------- reply unwrapping */

    #[test]
    fn strips_the_prose_data_get_wraps_its_answer_in() {
        let raw = "Steve has the following entity data: [{Slot: 0b, id: \"minecraft:stone\"}]";
        let body = strip_reply_prefix(raw).unwrap();
        assert!(body.starts_with('['));

        let items = list(&parse(body).unwrap()).clone();
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn a_scalar_reply_unwraps_too() {
        assert_eq!(
            strip_reply_prefix("Steve has the following entity data: 3"),
            Some("3")
        );
    }

    #[test]
    fn an_offline_player_yields_text_that_does_not_parse() {
        // The server answers this rather than erroring, so the caller has to be
        // able to tell it apart from real data — it must not silently read as
        // an empty inventory.
        let raw = "No entity was found";
        let body = strip_reply_prefix(raw).unwrap();
        assert!(
            parse(body).is_err(),
            "prose must not parse as an inventory"
        );
    }

    #[test]
    fn an_empty_reply_is_nothing_rather_than_an_empty_string() {
        assert_eq!(strip_reply_prefix(""), None);
        assert_eq!(strip_reply_prefix("   "), None);
    }

    /* ------------------------------------------------------- real payloads */

    #[test]
    fn reads_a_modern_inventory_reply() {
        let raw = concat!(
            "Steve has the following entity data: [",
            r#"{Slot: 0b, id: "minecraft:diamond_sword", count: 1, components: "#,
            r#"{"minecraft:enchantments": {levels: {"minecraft:sharpness": 5}}}}, "#,
            r#"{Slot: 1b, id: "minecraft:cobblestone", count: 64}]"#
        );

        let items = list(&parse(strip_reply_prefix(raw).unwrap()).unwrap()).clone();
        assert_eq!(items.len(), 2);

        let first = compound(&items[0]);
        assert_eq!(
            first.get("id"),
            Some(&Value::String("minecraft:diamond_sword".to_string()))
        );
        assert_eq!(first.get("count"), Some(&Value::Int(1)));
        assert_eq!(first.get("Slot"), Some(&Value::Byte(0)));
    }

    #[test]
    fn reads_a_legacy_inventory_reply() {
        let raw = concat!(
            "Steve has the following entity data: [",
            r#"{Slot: 0b, id: "minecraft:stone", Count: 64b, "#,
            r#"tag: {Damage: 0, display: {Name: '{"text":"Bonk"}'}}}]"#
        );

        let items = list(&parse(strip_reply_prefix(raw).unwrap()).unwrap()).clone();
        let tag = compound(compound(&items[0]).get("tag").unwrap());
        let display = compound(tag.get("display").unwrap());

        assert_eq!(
            display.get("Name"),
            Some(&Value::String(r#"{"text":"Bonk"}"#.to_string()))
        );
    }

    #[test]
    fn reads_an_equipment_reply() {
        let raw = concat!(
            "Steve has the following entity data: ",
            r#"{head: {id: "minecraft:netherite_helmet", count: 1}, "#,
            r#"offhand: {id: "minecraft:shield", count: 1}}"#
        );

        let map = compound(&parse(strip_reply_prefix(raw).unwrap()).unwrap()).clone();
        assert!(map.contains_key("head"));
        assert!(map.contains_key("offhand"));
    }
}
