//! Background lyric loading and the single current line in the playing bar.

use super::*;

pub(super) struct Loaded {
    generation: u64,
    result: Result<Option<crate::lyrics::Lyrics>, String>,
}

pub(super) struct Lyrics {
    enabled: bool,
    key: Option<String>,
    generation: u64,
    requested: bool,
    content: Option<crate::lyrics::Lyrics>,
    current: Option<usize>,
}

impl Lyrics {
    pub(super) fn new(enabled: bool) -> Self {
        Self {
            enabled,
            key: None,
            generation: 0,
            requested: false,
            content: None,
            current: None,
        }
    }

    /// Return true at a lyric boundary, including boundaries between the
    /// half-second progress-bar redraws. No requests while lyrics are hidden.
    pub(super) fn tick(
        &mut self,
        server: &Server,
        tx: &Sender<Msg>,
        st: &Status,
        visible: bool,
    ) -> bool {
        let key = st
            .track
            .as_ref()
            .filter(|_| st.state != PlayState::Stopped)
            .map(|t| t.key.as_str());
        if self.key.as_deref() != key {
            self.key = key.map(str::to_owned);
            self.generation = self.generation.wrapping_add(1);
            self.requested = false;
            self.content = None;
        }
        if self.enabled && visible && !self.requested {
            if let Some(key) = self.key.clone() {
                self.requested = true;
                let (server, tx, generation) = (server.clone(), tx.clone(), self.generation);
                thread::spawn(move || {
                    let result = server.lyrics(&key).map_err(|e| e.to_string());
                    let _ = tx.send(Msg::Lyrics(Loaded { generation, result }));
                });
            }
        }
        let current = visible.then(|| self.line(st).map(|(i, _)| i)).flatten();
        let changed = self.current != current;
        self.current = current;
        changed
    }

    pub(super) fn loaded(&mut self, loaded: Loaded) {
        // A slow answer must not replace a newer song's lyrics, even when
        // the user has since returned to the original song (A -> B -> A).
        if loaded.generation != self.generation {
            return;
        }
        match loaded.result {
            Ok(content) => self.content = content,
            Err(error) => crate::log::write(&format!("lyrics: {}", error)),
        }
    }

    pub(super) fn line(&self, st: &Status) -> Option<(usize, &str)> {
        if !self.enabled
            || st.state == PlayState::Stopped
            || self.key.as_deref() != st.track.as_ref().map(|t| t.key.as_str())
        {
            return None;
        }
        self.content.as_ref()?.line(st.pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plex::Plex;
    use ratatui::{backend::TestBackend, Terminal};
    use serde_json::json;

    fn status(key: &str, pos: f64) -> Status {
        Status {
            track: Some(
                serde_json::from_value(
                    json!({"key": key, "title": "Song", "artist": "Artist", "album": "Album"}),
                )
                .unwrap(),
            ),
            state: PlayState::Playing,
            pos,
            dur: Some(60.0),
            ..Status::default()
        }
    }

    fn server() -> Server {
        Server::new(Plex::new("test"), "http://127.0.0.1:9", "token", "machine")
    }

    fn ready() -> Lyrics {
        Lyrics {
            enabled: true,
            key: Some("1".into()),
            generation: 1,
            requested: true,
            content: crate::lyrics::Lyrics::parse(
                "[00:01.100]First lyric\n[00:01.300]Second lyric\n[00:04]",
            ),
            current: None,
        }
    }

    #[test]
    fn lyric_boundaries_trigger_redraws_between_progress_updates_and_follow_seeks() {
        let mut lyrics = ready();
        let (tx, _rx) = mpsc::channel();
        let server = server();
        assert!(lyrics.tick(&server, &tx, &status("1", 1.1), true));
        assert!(!lyrics.tick(&server, &tx, &status("1", 1.2), true));
        assert!(lyrics.tick(&server, &tx, &status("1", 1.3), true));
        let mut paused = status("1", 1.3);
        paused.state = PlayState::Paused;
        assert!(!lyrics.tick(&server, &tx, &paused, true));
        assert_eq!(lyrics.line(&paused).unwrap().1, "Second lyric");
        assert!(lyrics.tick(&server, &tx, &status("1", 1.1), true));
        assert_eq!(lyrics.line(&status("1", 1.1)).unwrap().1, "First lyric");
        assert!(lyrics.tick(&server, &tx, &status("1", 4.0), true));
        assert!(lyrics.line(&status("1", 4.0)).is_none());
    }

    #[test]
    fn hidden_and_disabled_lyrics_do_not_request_or_reuse_a_previous_songs_lines() {
        let (tx, rx) = mpsc::channel();
        let server = server();
        let mut hidden = Lyrics::new(true);
        hidden.tick(&server, &tx, &status("1", 2.0), false);
        assert!(!hidden.requested);
        let mut disabled = Lyrics::new(false);
        disabled.tick(&server, &tx, &status("1", 2.0), true);
        assert!(!disabled.requested);
        assert!(rx.try_recv().is_err());

        let mut lyrics = ready();
        assert!(lyrics.line(&status("2", 2.0)).is_none());
        lyrics.tick(&server, &tx, &status("2", 2.0), false);
        lyrics.tick(&server, &tx, &status("1", 2.0), false);
        lyrics.loaded(Loaded {
            generation: 1,
            result: Ok(crate::lyrics::Lyrics::parse("[00:00]Stale lyric")),
        });
        assert!(lyrics.line(&status("1", 2.0)).is_none());
        lyrics.tick(&server, &tx, &Status::default(), true);
        assert!(lyrics.line(&Status::default()).is_none());
    }

    #[test]
    fn one_background_request_is_kept_for_a_track_including_when_none_are_available() {
        let body = json!({"MediaContainer": {"Metadata": [{"Media": [{"Part": [{"Stream": [
            {"streamType": 4, "format": "lrc", "key": "/lyrics"}
        ]}]}]}]}});
        let (base, log) = crate::test_server::start(vec![
            ("GET", "/library/metadata/1", body),
            (
                "GET",
                "/lyrics",
                json!({"MediaContainer": {"Lyrics": [{"Line": [
                    {"Span": [{"startOffset": 1000, "text": "Loaded lyric"}]}
                ]}]}}),
            ),
        ]);
        let server = Server::new(Plex::new("test"), &base, "token", "machine");
        let (tx, rx) = mpsc::channel();
        let mut lyrics = Lyrics::new(true);
        let st = status("1", 2.0);
        lyrics.tick(&server, &tx, &st, true);
        for _ in 0..5 {
            lyrics.tick(&server, &tx, &st, true);
        }
        let Msg::Lyrics(loaded) = rx.recv_timeout(Duration::from_secs(5)).unwrap() else {
            panic!("expected lyrics")
        };
        lyrics.loaded(loaded);
        assert_eq!(lyrics.line(&st).unwrap().1, "Loaded lyric");
        lyrics.tick(&server, &tx, &st, false);
        lyrics.tick(&server, &tx, &st, true);
        assert_eq!(log.lock().unwrap().len(), 2);

        let st = status("2", 2.0);
        lyrics.tick(&server, &tx, &st, true);
        let Msg::Lyrics(loaded) = rx.recv_timeout(Duration::from_secs(5)).unwrap() else {
            panic!("expected lyrics")
        };
        lyrics.loaded(loaded);
        assert!(lyrics.line(&st).is_none());
        for _ in 0..5 {
            lyrics.tick(&server, &tx, &st, true);
        }
        assert_eq!(log.lock().unwrap().len(), 3);
    }

    #[test]
    fn only_the_current_line_is_centered_in_the_cover_layout() {
        let mut app = App::new(
            server(),
            "1".into(),
            HeaderInfo {
                server_name: "Home".into(),
                library_title: "Music".into(),
            },
            crate::output::Target::Alsa("null".into()),
            false,
            true,
            false,
            false,
            true,
        );
        app.lyrics = ready();
        for (width, height, cover, enabled, state, want) in [
            (140, 40, true, true, PlayState::Playing, true),
            (90, 30, true, true, PlayState::Paused, true),
            (89, 40, true, true, PlayState::Playing, false),
            (140, 29, true, true, PlayState::Playing, false),
            (140, 40, false, true, PlayState::Playing, false),
            (140, 40, true, false, PlayState::Playing, false),
            (140, 40, true, true, PlayState::Stopped, false),
        ] {
            app.album_art = cover;
            app.lyrics.enabled = enabled;
            let mut st = status("1", 1.3);
            st.state = state;
            let mut term = Terminal::new(TestBackend::new(width, height)).unwrap();
            term.draw(|f| app.draw(f, &st)).unwrap();
            let buf = term.backend().buffer();
            let rows: Vec<String> = (0..height)
                .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect())
                .collect();
            assert_eq!(
                rows.iter().filter(|r| r.contains("Second lyric")).count(),
                usize::from(want),
                "{width}x{height} art={cover} enabled={enabled}"
            );
            assert!(!rows.iter().any(|r| r.contains("First lyric")));
            if want {
                let y = height - 1 - NOW_PLAYING_TALL_H + 1;
                let row = &rows[(y + 3) as usize];
                let text_x = 2 + app.art_cols as u16 + 2;
                let text_width = width - 4 - app.art_cols as u16 - 2;
                let start = row.find("Second lyric").unwrap();
                assert_eq!(
                    row[..start].chars().count(),
                    (text_x + (text_width - 12) / 2) as usize
                );
                assert!(rows[(y + 5) as usize].contains("0:01"));
            }
        }
    }
}
