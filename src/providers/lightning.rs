use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::cache::write_log;
use crate::geo::{lat_lon_to_world, WorldPoint};

const WS_SERVERS: &[&str] = &[
    "wss://ws1.blitzortung.org",
    "wss://ws7.blitzortung.org",
    "wss://ws8.blitzortung.org",
];

/// Sentinel sent to the channel to signal a successful WS handshake.
/// x < 0 is outside the valid [0, 1]² world range; polarity field is ignored.
pub const CONNECTED_SENTINEL: WorldPoint = WorldPoint { x: -1.0, y: -1.0 };

/// LZW-variant decode used by blitzortung.org websocket frames.
///
/// Ported verbatim from https://github.com/akeamc/blitzortung (MIT).
fn decode(ciphertext: &str) -> String {
    let mut chars = ciphertext.chars();
    let Some(mut c) = chars.next() else {
        return String::new();
    };
    let mut prev = c.to_string();
    let mut out = c.to_string();
    let mut dict = Vec::<String>::with_capacity(ciphertext.len());

    for ch in chars {
        let code = ch as usize;
        let a = if code < 256 {
            ch.to_string()
        } else {
            dict.get(code - 256)
                .cloned()
                .unwrap_or_else(|| format!("{prev}{c}"))
        };
        out.push_str(&a);
        c = a.chars().next().unwrap();
        dict.push(format!("{prev}{c}"));
        prev = a;
    }
    out
}

#[derive(Deserialize)]
struct RawStrike {
    lat: f64,
    lon: f64,
    pol: i32,
}

/// Connect to a random Blitzortung WS server, subscribe, and forward
/// `(WorldPoint, polarity)` pairs to `tx`.
///
/// Sends `(CONNECTED_SENTINEL, 0)` after each successful handshake.
/// Reconnects automatically on drops.  Returns when:
/// - `close_rx` fires (caller signals graceful shutdown — a WS Close frame
///   is sent before returning), or
/// - `cancel` is set (abort path — Close frame still attempted), or
/// - `tx` is closed.
pub async fn connect_and_stream(
    tx: UnboundedSender<(WorldPoint, i8)>,
    log: PathBuf,
    cancel: Arc<AtomicBool>,
    mut close_rx: oneshot::Receiver<()>,
) {
    let mut server_idx = 0usize;

    loop {
        if cancel.load(Ordering::Relaxed) {
            return;
        }

        let server = WS_SERVERS[server_idx % WS_SERVERS.len()];
        server_idx += 1;

        write_log(&log, format!("lightning: connecting to {server}"));

        let ws = match connect_async(server).await {
            Ok((ws, _)) => ws,
            Err(e) => {
                write_log(&log, format!("lightning: connect failed: {e}"));
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };

        let (mut sink, mut stream) = ws.split();

        if let Err(e) = sink.send(Message::Text(r#"{"a":111}"#.to_string())).await {
            write_log(&log, format!("lightning: subscribe send failed: {e}"));
            tokio::time::sleep(Duration::from_secs(2)).await;
            continue;
        }

        write_log(&log, "lightning: connected, streaming");
        if tx.send((CONNECTED_SENTINEL, 0)).is_err() {
            return;
        }

        loop {
            tokio::select! {
                biased;

                // Graceful close requested by the caller.
                _ = &mut close_rx => {
                    let _ = sink.send(Message::Close(None)).await;
                    return;
                }

                msg_result = stream.next() => {
                    if cancel.load(Ordering::Relaxed) {
                        let _ = sink.send(Message::Close(None)).await;
                        return;
                    }
                    let text = match msg_result {
                        Some(Ok(Message::Text(t))) => t,
                        Some(Ok(Message::Close(_))) | None => break,
                        Some(Ok(_)) => continue,
                        Some(Err(e)) => {
                            write_log(&log, format!("lightning: ws error: {e}"));
                            break;
                        }
                    };

                    let json = decode(&text);
                    let Ok(strike) = serde_json::from_str::<RawStrike>(&json) else {
                        continue;
                    };

                    let world = lat_lon_to_world(strike.lat, strike.lon);
                    let pol = strike.pol.clamp(-127, 127) as i8;
                    if tx.send((world, pol)).is_err() {
                        return;
                    }
                }
            }
        }

        write_log(&log, "lightning: disconnected, reconnecting");
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_roundtrip_ascii() {
        let s = r#"{"lat":1.0,"lon":2.0}"#;
        assert_eq!(decode(s), s);
    }

    #[test]
    fn decode_empty() {
        assert_eq!(decode(""), "");
    }

    #[test]
    fn decode_single_char() {
        assert_eq!(decode("x"), "x");
    }

    // Manually-constructed LZW test vector.
    // Decode algorithm ported from https://github.com/akeamc/blitzortung (MIT).
    // "ab" followed by U+0100 (code 256 → dict[0] = "ab") decodes to "abab".
    #[test]
    fn decode_with_dict_lookup_produces_repeated_pair() {
        let encoded = "ab\u{0100}";
        assert_eq!(decode(encoded), "abab");
    }

    // Second dict entry: "ab" + U+0100 (="ab") + U+0101 (dict[1]="ba") → "ababba"
    #[test]
    fn decode_with_two_dict_lookups() {
        let encoded = "ab\u{0100}\u{0101}";
        assert_eq!(decode(encoded), "ababba");
    }

    // A valid JSON strike payload that survives the decode identity pass.
    #[test]
    fn decode_strike_json_identity() {
        let json = r#"{"lat":48.21,"lon":16.37,"pol":1}"#;
        let decoded = decode(json);
        let strike: RawStrike = serde_json::from_str(&decoded).unwrap();
        assert!((strike.lat - 48.21).abs() < 0.001);
        assert!((strike.lon - 16.37).abs() < 0.001);
        assert_eq!(strike.pol, 1);
    }

    #[test]
    fn decode_strike_json_negative_polarity() {
        let json = r#"{"lat":0.0,"lon":0.0,"pol":-1}"#;
        let decoded = decode(json);
        let strike: RawStrike = serde_json::from_str(&decoded).unwrap();
        assert_eq!(strike.pol, -1);
    }
}
