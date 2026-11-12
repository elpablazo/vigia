//! Compact history: prices are stored only when they change (one ~30-byte row per change, kept
//! forever), page text is deflated. This module reads that back for analysis.

use rusqlite::types::ValueRef;
use serde_json::{Value, json};

pub fn pack(text: &str) -> Vec<u8> {
    miniz_oxide::deflate::compress_to_vec(text.as_bytes(), 8)
}

/// Text snapshots written before compression existed are plain TEXT; newer ones are deflated BLOBs.
pub fn unpack(v: ValueRef) -> Option<String> {
    match v {
        ValueRef::Text(t) => Some(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => miniz_oxide::inflate::decompress_to_vec_with_limit(b, 8_000_000)
            .ok()
            .map(|v| String::from_utf8_lossy(&v).into_owned()),
        _ => None,
    }
}

/// Summary of a step series (a price holds until the next change) over `[since, until]`.
/// `points` are (unix time, price) in ascending time. The average is weighted by how long each price lasted.
pub fn stats(points: &[(i64, f64)], since: Option<i64>, until: i64) -> Option<Value> {
    let first = points.first()?.0;
    let start = since.map_or(first, |s| s.max(first));
    let (mut min, mut max) = ((f64::MAX, start), (f64::MIN, start));
    let (mut weighted, mut span, mut changes, mut last) = (0.0, 0i64, 0, 0.0);
    for (i, &(at, price)) in points.iter().enumerate() {
        let end = points.get(i + 1).map_or(until, |p| p.0).min(until);
        if end <= start && i + 1 < points.len() {
            continue; // superseded before the window opened
        }
        let from = at.max(start);
        if at > start {
            changes += 1;
        }
        if price < min.0 {
            min = (price, from);
        }
        if price > max.0 {
            max = (price, from);
        }
        weighted += price * (end - from).max(0) as f64;
        span += (end - from).max(0);
        last = price;
    }
    let avg = if span > 0 { weighted / span as f64 } else { last };
    Some(json!({
        "since": start, "min": min.0, "min_at": min.1, "max": max.0, "max_at": max.1,
        "avg": (avg * 100.0).round() / 100.0, "changes": changes, "last": last,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weighted_stats() {
        let pts = [(0, 100.0), (10, 50.0), (20, 100.0)];
        let all = stats(&pts, None, 30).unwrap();
        assert_eq!((all["min"].as_f64(), all["min_at"].as_i64(), all["max"].as_f64()), (Some(50.0), Some(10), Some(100.0)));
        assert_eq!((all["avg"].as_f64(), all["changes"].as_i64()), (Some(83.33), Some(2)));
        // Window opens while 50 is in effect: that price counts from the window start.
        let w = stats(&pts, Some(15), 30).unwrap();
        assert_eq!(
            (w["min"].as_f64(), w["min_at"].as_i64(), w["avg"].as_f64(), w["changes"].as_i64()),
            (Some(50.0), Some(15), Some(83.33), Some(1))
        );
        // Window after the last change: only the current price.
        let late = stats(&pts, Some(25), 30).unwrap();
        assert_eq!((late["min"].as_f64(), late["max"].as_f64(), late["changes"].as_i64()), (Some(100.0), Some(100.0), Some(0)));
        assert!(stats(&[], None, 30).is_none());
    }

    #[test]
    fn text_roundtrip() {
        let t = "línea uno\n".repeat(500);
        let packed = pack(&t);
        assert!(packed.len() < t.len() / 20);
        assert_eq!(unpack(ValueRef::Blob(&packed)).as_deref(), Some(t.as_str()));
        assert_eq!(unpack(ValueRef::Text(b"viejo")).as_deref(), Some("viejo"));
    }
}
