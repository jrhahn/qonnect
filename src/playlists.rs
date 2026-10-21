//! `qonnect dump`: every playlist of the user with its tracks, as one JSON document on stdout.
//! `qonnect add`: tracks named as `Artist - Title`, one per line on stdin, appended to a playlist.
//!
//! The dump is trimmed to what tells a playlist apart — artist, title, album, genre, year — so the whole
//! library fits in front of a reader, human or model, instead of the kilobytes Qobuz sends per
//! track.

use serde_json::{json, Value};

use crate::qobuz::Qobuz;

/// Qobuz caps a page at 500 items.
const PAGE: usize = 500;

pub async fn dump(qobuz: &Qobuz) -> Result<(), String> {
    let mut playlists = Vec::new();
    for playlist in pages(qobuz, "playlist/getUserPlaylists", &[], "playlists").await? {
        let Some(id) = playlist.get("id").map(|id| id.to_string().replace('"', "")) else {
            continue;
        };
        let tracks = pages(
            qobuz,
            "playlist/get",
            &[("playlist_id", &id), ("extra", "tracks")],
            "tracks",
        )
        .await?;
        playlists.push(json!({
            "id": id,
            "name": playlist.get("name"),
            "description": playlist.get("description"),
            "tracks": tracks.iter().map(track).collect::<Vec<_>>(),
        }));
    }
    let out = serde_json::to_string_pretty(&playlists).map_err(|err| err.to_string())?;
    println!("{out}");
    Ok(())
}

/// Looks every line up in the catalogue and appends what it finds, skipping tracks the playlist
/// already holds. A line it cannot place is reported, not guessed.
pub async fn add(qobuz: &Qobuz, playlist: &str, dry_run: bool) -> Result<(), String> {
    let held: Vec<u64> = pages(
        qobuz,
        "playlist/get",
        &[("playlist_id", playlist), ("extra", "tracks")],
        "tracks",
    )
    .await?
    .iter()
    .filter_map(|item| item.get("id")?.as_u64())
    .collect();
    let mut ids = Vec::new();
    for line in std::io::stdin().lines() {
        let line = line.map_err(|err| err.to_string())?;
        let Some((artist, title)) = line.split_once(" - ") else {
            if !line.trim().is_empty() {
                eprintln!("skipped, not `Artist - Title`: {line}");
            }
            continue;
        };
        let (artist, title) = (artist.trim(), title.trim());
        let query = format!("{artist} {title}");
        let body = qobuz
            .get("track/search", &[("query", &query), ("limit", "10")])
            .await
            .map_err(|err| format!("track/search: {err}"))?;
        let found = body
            .pointer("/tracks/items")
            .and_then(Value::as_array)
            .and_then(|items| pick(items, artist, title));
        match found {
            Some(item) => {
                let id = item.get("id").and_then(Value::as_u64).unwrap_or_default();
                let t = track(item);
                let what = format!("{} - {} [{}]", t["artist"], t["title"], t["album"]);
                if held.contains(&id) || ids.contains(&id) {
                    eprintln!("already there: {what}");
                } else {
                    eprintln!("found: {what}");
                    ids.push(id);
                }
            }
            None => eprintln!("NOT FOUND: {line}"),
        }
    }
    if ids.is_empty() || dry_run {
        eprintln!("{} to add, nothing written", ids.len());
        return Ok(());
    }
    let ids = ids.iter().map(u64::to_string).collect::<Vec<_>>().join(",");
    qobuz
        .post(
            "playlist/addTracks",
            &[("playlist_id", playlist), ("track_ids", &ids)],
        )
        .await
        .map_err(|err| format!("playlist/addTracks: {err}"))?;
    eprintln!("added {} tracks", ids.split(',').count());
    Ok(())
}

/// The first search hit by that artist whose title starts with the wanted one, so `Teardrop`
/// takes `Teardrop (Remastered)` but not a cover or `Teardrop Explodes`-style namesake.
fn pick<'a>(items: &'a [Value], artist: &str, title: &str) -> Option<&'a Value> {
    let fold = |text: &str| text.to_lowercase().replace(['’', '‘'], "'");
    let (artist, title) = (fold(artist), fold(title));
    items.iter().find(|item| {
        let by = ["/performer/name", "/album/artist/name"]
            .iter()
            .filter_map(|at| item.pointer(at).and_then(Value::as_str))
            .any(|name| fold(name) == artist);
        let named = item
            .get("title")
            .and_then(Value::as_str)
            .map(fold)
            .is_some_and(|name| {
                name == title
                    || name
                        .strip_prefix(&title)
                        .is_some_and(|rest| rest.starts_with([' ', '(']))
            });
        by && named
    })
}

/// Every item under `key`, following `offset` until a page comes back short.
async fn pages(
    qobuz: &Qobuz,
    path: &str,
    query: &[(&str, &str)],
    key: &str,
) -> Result<Vec<Value>, String> {
    let mut all = Vec::new();
    loop {
        let offset = all.len().to_string();
        let limit = PAGE.to_string();
        let mut query = query.to_vec();
        query.extend([("offset", offset.as_str()), ("limit", limit.as_str())]);
        let body = qobuz
            .get(path, &query)
            .await
            .map_err(|err| format!("{path}: {err}"))?;
        let page = body
            .pointer(&format!("/{key}/items"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let short = page.len() < PAGE;
        all.extend(page);
        if short {
            return Ok(all);
        }
    }
}

/// One track, trimmed. `id` stays so a later step can refer to the exact recording.
fn track(item: &Value) -> Value {
    let text = |at: &str| item.pointer(at).and_then(Value::as_str);
    let title = match (text("/title"), text("/version")) {
        (Some(title), Some(version)) if !version.is_empty() => format!("{title} ({version})"),
        (title, _) => title.unwrap_or_default().to_owned(),
    };
    json!({
        "id": item.get("id"),
        "artist": text("/performer/name").or(text("/album/artist/name")),
        "title": title,
        "album": text("/album/title"),
        "genre": text("/album/genre/name"),
        "year": text("/album/release_date_original").and_then(|date| date.get(..4)),
        "seconds": item.get("duration"),
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{pick, track};

    #[test]
    fn picks_the_artist_and_title_not_a_namesake() {
        let hits = [
            json!({ "id": 1, "title": "Teardrop", "performer": { "name": "Some Cover Band" } }),
            json!({ "id": 2, "title": "Teardrops", "performer": { "name": "Massive Attack" } }),
            json!({ "id": 3, "title": "Teardrop (Remastered)", "performer": { "name": "Massive Attack" } }),
        ];
        assert_eq!(pick(&hits, "massive attack", "Teardrop").unwrap()["id"], 3);
        assert!(pick(&hits, "Portishead", "Roads").is_none());
    }

    #[test]
    fn trims_a_track_to_what_tells_it_apart() {
        let item = json!({
            "id": 42, "title": "Columbo", "version": "Live", "duration": 227,
            "performer": { "name": "Wanda" },
            "album": {
                "title": "Amore", "artist": { "name": "Various" },
                "genre": { "name": "Pop" }, "release_date_original": "2014-09-26",
                "image": { "large": "https://…" },
            },
        });
        assert_eq!(
            track(&item),
            json!({
                "id": 42, "artist": "Wanda", "title": "Columbo (Live)", "album": "Amore",
                "genre": "Pop", "year": "2014", "seconds": 227,
            })
        );
    }
}
