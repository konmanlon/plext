//! Timed lyrics returned by Plex, kept as a single line at the playback position.

use serde_json::Value;

#[derive(Debug)]
struct Cue {
    start: i64,
    end: Option<i64>,
    text: String,
}

#[derive(Debug)]
pub struct Lyrics {
    cues: Vec<Cue>,
}

impl Lyrics {
    /// Plex may return structured JSON or the original LRC file. Untimed
    /// text has no current line, so it is deliberately left undisplayed.
    pub fn parse(body: &str) -> Option<Self> {
        let body = body.trim_start_matches('\u{feff}').trim();
        let mut cues = if body.starts_with('{') {
            Self::json(&serde_json::from_str::<Value>(body).ok()?)
        } else {
            Self::lrc(body)
        };
        if !cues.iter().any(|c| !c.text.is_empty()) {
            return None;
        }
        cues.sort_by_key(|c| c.start);
        Some(Self { cues })
    }

    /// Find the line afresh, so pause, seeking, repeats and late downloads
    /// all use the player's position rather than a separate lyrics clock.
    pub fn line(&self, seconds: f64) -> Option<(usize, &str)> {
        if !seconds.is_finite() || seconds < 0.0 {
            return None;
        }
        let index = self
            .cues
            .partition_point(|c| c.start as f64 / 1000.0 <= seconds)
            .checked_sub(1)?;
        let cue = &self.cues[index];
        if cue.text.is_empty() || cue.end.is_some_and(|end| seconds >= end as f64 / 1000.0) {
            return None;
        }
        Some((index, &cue.text))
    }

    fn json(value: &Value) -> Vec<Cue> {
        let mut cues = Vec::new();
        let Some(lines) = value
            .pointer("/MediaContainer/Lyrics/0/Line")
            .and_then(Value::as_array)
        else {
            return cues;
        };
        for line in lines {
            let Some(spans) = line["Span"].as_array() else {
                continue;
            };
            let Some(start) = spans.first().and_then(|s| milliseconds(&s["startOffset"])) else {
                continue;
            };
            let text: String = spans.iter().filter_map(|s| s["text"].as_str()).collect();
            let end = spans
                .last()
                .and_then(|s| milliseconds(&s["endOffset"]))
                .filter(|&end| end > start);
            cues.push(Cue {
                start,
                end,
                text: single_line(&text),
            });
        }
        cues
    }

    fn lrc(body: &str) -> Vec<Cue> {
        let mut cues = Vec::new();
        let mut offset = 0_i64;
        for line in body.lines() {
            let mut rest = line.trim();
            if let Some(tag) = rest.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                if let Some((name, value)) = tag.split_once(':') {
                    if name.eq_ignore_ascii_case("offset") {
                        if let Ok(value) = value.trim().parse() {
                            offset = value;
                        }
                        continue;
                    }
                }
            }
            let mut times = Vec::new();
            while let Some(tag) = rest.strip_prefix('[') {
                let Some((time, tail)) = tag.split_once(']') else {
                    break;
                };
                let Some(time) = timestamp(time) else { break };
                times.push(time);
                rest = tail;
            }
            let text = single_line(rest);
            for start in times {
                // Empty timed lines clear the display during instrumental gaps.
                cues.push(Cue {
                    start,
                    end: None,
                    text: text.clone(),
                });
            }
        }
        for cue in &mut cues {
            cue.start = cue.start.saturating_add(offset);
        }
        cues
    }
}

fn milliseconds(value: &Value) -> Option<i64> {
    value.as_i64().or_else(|| value.as_str()?.parse().ok())
}

fn single_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn timestamp(text: &str) -> Option<i64> {
    let (minutes, seconds) = text.split_once(':')?;
    let (seconds, fraction) = seconds.split_once('.').unwrap_or((seconds, ""));
    if minutes.is_empty()
        || seconds.is_empty()
        || !minutes.bytes().all(|c| c.is_ascii_digit())
        || !seconds.bytes().all(|c| c.is_ascii_digit())
        || fraction.len() > 3
        || !fraction.bytes().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    let seconds: i64 = seconds.parse().ok()?;
    if seconds >= 60 {
        return None;
    }
    let fraction = if fraction.is_empty() {
        0
    } else {
        fraction.parse::<i64>().ok()? * 10_i64.pow(3 - fraction.len() as u32)
    };
    minutes
        .parse::<i64>()
        .ok()?
        .checked_mul(60_000)?
        .checked_add(seconds * 1000 + fraction)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn lrc_follows_timestamps_in_both_directions_and_clears_gaps() {
        let lyrics = Lyrics::parse(
            "\u{feff}[ar:Artist]\r\n[00:03.125]Second\r\n[00:01.5][00:06]First\r\n[00:04]\r\n",
        )
        .unwrap();
        assert_eq!(lyrics.line(1.499), None);
        assert_eq!(lyrics.line(1.5).unwrap().1, "First");
        assert_eq!(lyrics.line(3.125).unwrap().1, "Second");
        assert_eq!(lyrics.line(4.0), None);
        assert_eq!(lyrics.line(6.0).unwrap().1, "First");
        assert_eq!(lyrics.line(2.0).unwrap().1, "First");
        assert_eq!(lyrics.line(0.0), None);
        assert_eq!(lyrics.line(f64::NAN), None);
    }

    #[test]
    fn lrc_offset_applies_even_when_the_tag_is_last() {
        let lyrics = Lyrics::parse("[00:01.00]Early\n[00:02.00]Later\n[offset:-250]").unwrap();
        assert_eq!(lyrics.line(0.749), None);
        assert_eq!(lyrics.line(0.75).unwrap().1, "Early");
        assert_eq!(lyrics.line(1.75).unwrap().1, "Later");
        let lyrics = Lyrics::parse("[offset:+500]\n[00:00.10]Delayed").unwrap();
        assert_eq!(lyrics.line(0.599), None);
        assert_eq!(lyrics.line(0.6).unwrap().1, "Delayed");
    }

    #[test]
    fn plex_json_joins_spans_and_honors_the_end_of_a_line() {
        let body = json!({"MediaContainer": {"Lyrics": [{"Line": [
            {"Span": [{"startOffset": 1500, "text": "Evening "}, {"startOffset": 1700, "endOffset": 2500, "text": "breeze"}]},
            {"Span": [{"startOffset": "3000", "text": "Next\nline"}]},
            {"Span": [{"startOffset": 5000, "text": ""}]}
        ]}]}});
        let lyrics = Lyrics::parse(&body.to_string()).unwrap();
        assert_eq!(lyrics.line(1.0), None);
        assert_eq!(lyrics.line(1.5).unwrap().1, "Evening breeze");
        assert_eq!(lyrics.line(2.5), None);
        assert_eq!(lyrics.line(3.0).unwrap().1, "Next line");
        assert_eq!(lyrics.line(5.0), None);
    }

    #[test]
    fn empty_untimed_and_malformed_lyrics_have_nothing_to_display() {
        for body in [
            "",
            "Plain lyrics\nwithout timing",
            "[ar:Artist]",
            "[bad]Text",
            "[00:99]Bad",
            "[00:01]",
            "{bad json",
            r#"{"MediaContainer":{"Lyrics":[]}}"#,
            r#"{"MediaContainer":{"Lyrics":[{"Line":[{"Span":[{"text":"Untimed"}]}]}]}}"#,
        ] {
            assert!(Lyrics::parse(body).is_none(), "{body}");
        }
        // A bad timestamp must not prevent later valid lines from being used.
        let lyrics = Lyrics::parse("[99:bad]Bad\n[00:01.001]Valid").unwrap();
        assert_eq!(lyrics.line(1.001).unwrap().1, "Valid");
    }
}
