//! Pauses the media players whose streams lost their outputs on another
//! node (`receiver.when_lost = "pause"`), through MPRIS on the session bus.
//!
//! Strictly best-effort: a player is paused only if the audio system can
//! tell for sure which process plays into a lost route's sink, that process
//! has the player, and every stream it plays goes to outputs that are lost;
//! one that also plays elsewhere keeps playing, and its lost stream stays
//! parked. Players go by process ID, never by name: the process itself, or
//! the nearest parent running the same program, as browsers play from a
//! helper process.

use std::collections::{BTreeSet, HashSet};
use std::time::Duration;

use orpheus_engine::AppStream;
use zbus::Connection;
use zbus::OwnedVariant;

/// The bus names of MPRIS players start with this.
const PREFIX: &str = "org.mpris.MediaPlayer2.";
const PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER: &str = "org.mpris.MediaPlayer2.Player";
/// How long a player may take to answer.
const TIMEOUT: Duration = Duration::from_secs(2);

/// Pauses the players of the processes in `streams` that play into the
/// outputs in `lost`, which just went, and nowhere but into those in
/// `unavailable`, which includes `lost`.
pub async fn pause(streams: Vec<AppStream>, lost: HashSet<String>, unavailable: HashSet<String>) {
    let hits = |s: &AppStream| s.outputs.iter().any(|o| lost.contains(o));
    tracing::debug!(?streams, ?lost, "outputs lost");
    if !streams.iter().any(hits) {
        return;
    }
    let bus = match Connection::session().await {
        Ok(bus) => bus,
        Err(e) => {
            tracing::debug!(error = %e, "no session bus; pausing no player");
            return;
        }
    };
    let players = match players(&bus).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "listing the media players");
            return;
        }
    };
    let owners: HashSet<u32> = players.iter().map(|(_, pid)| *pid).collect();
    let streams: Vec<(Option<u32>, &[String])> = streams
        .iter()
        .map(|s| {
            let owner = s.process.and_then(|p| owner(p, &owners));
            if owner.is_none() && hits(s) {
                tracing::info!(
                    process = s.process,
                    "can't tell which player plays into the lost output; its stream stays parked"
                );
            }
            (owner, s.outputs.as_slice())
        })
        .collect();
    let chosen = choose(&streams, &lost, &unavailable);
    let elsewhere: BTreeSet<u32> = streams
        .iter()
        .filter(|(_, outputs)| outputs.iter().any(|o| lost.contains(o)))
        .filter_map(|(process, _)| *process)
        .filter(|p| !chosen.contains(p))
        .collect();
    for process in elsewhere {
        tracing::info!(
            process,
            "the player plays on other outputs too; it goes on playing, its lost stream stays parked"
        );
    }
    for process in chosen {
        for (name, _) in players.iter().filter(|(_, pid)| *pid == process) {
            match playing(&bus, name).await {
                Ok(true) => match call(&bus, name, PLAYER, "Pause", &()).await {
                    Ok(_) => tracing::info!(player = name, "paused: its output is gone"),
                    Err(e) => tracing::warn!(player = name, error = %e, "pausing"),
                },
                Ok(false) => {}
                Err(e) => tracing::warn!(player = name, error = %e, "asking whether it plays"),
            }
        }
    }
}

/// The processes to pause, given each stream's player process and outputs:
/// those with a stream into `lost` whose streams all play nowhere but into
/// `unavailable`.
fn choose(
    streams: &[(Option<u32>, &[String])],
    lost: &HashSet<String>,
    unavailable: &HashSet<String>,
) -> BTreeSet<u32> {
    let hit: BTreeSet<u32> = streams
        .iter()
        .filter(|(_, outputs)| outputs.iter().any(|o| lost.contains(o)))
        .filter_map(|(process, _)| *process)
        .collect();
    hit.into_iter()
        .filter(|&p| {
            streams
                .iter()
                .filter(|(q, _)| *q == Some(p))
                .all(|(_, outputs)| outputs.iter().all(|o| unavailable.contains(o)))
        })
        .collect()
}

/// The process among `owners` (those with players) whose player plays
/// what `process` plays: itself, or its nearest parent running the same
/// program.
fn owner(process: u32, owners: &HashSet<u32>) -> Option<u32> {
    if owners.contains(&process) {
        return Some(process);
    }
    let same = program(process)?;
    let mut p = process;
    loop {
        p = parent(p).filter(|&p| p > 1)?;
        if program(p)? != same {
            return None;
        }
        if owners.contains(&p) {
            return Some(p);
        }
    }
}

fn program(process: u32) -> Option<std::path::PathBuf> {
    std::fs::read_link(format!("/proc/{process}/exe")).ok()
}

fn parent(process: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{process}/stat")).ok()?;
    // `pid (comm) state ppid …`, where comm may hold anything.
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// The MPRIS players on the bus and the process of each.
async fn players(bus: &Connection) -> zbus::Result<Vec<(String, u32)>> {
    let reply = call(
        bus,
        "org.freedesktop.DBus",
        "org.freedesktop.DBus",
        "ListNames",
        &(),
    )
    .await?;
    let names: Vec<String> = reply.body().deserialize()?;
    let mut players = Vec::new();
    for name in names.into_iter().filter(|n| n.starts_with(PREFIX)) {
        let reply = call(
            bus,
            "org.freedesktop.DBus",
            "org.freedesktop.DBus",
            "GetConnectionUnixProcessID",
            &(name.as_str(),),
        )
        .await;
        // A player may have gone meanwhile.
        if let Ok(pid) = reply.and_then(|r| r.body().deserialize::<u32>()) {
            players.push((name, pid));
        }
    }
    Ok(players)
}

/// Whether player `name` says it is playing.
async fn playing(bus: &Connection, name: &str) -> zbus::Result<bool> {
    let reply = call(
        bus,
        name,
        "org.freedesktop.DBus.Properties",
        "Get",
        &(PLAYER, "PlaybackStatus"),
    )
    .await?;
    let status: OwnedVariant = reply.body().deserialize()?;
    Ok(String::try_from(status).is_ok_and(|s| s == "Playing"))
}

async fn call<B>(
    bus: &Connection,
    destination: &str,
    interface: &str,
    method: &str,
    body: &B,
) -> zbus::Result<zbus::Message>
where
    B: serde::Serialize + zbus::DynamicType,
{
    let path = if destination == "org.freedesktop.DBus" {
        "/org/freedesktop/DBus"
    } else {
        PATH
    };
    tokio::time::timeout(
        TIMEOUT,
        bus.call_method(Some(destination), path, Some(interface), method, body),
    )
    .await
    .map_err(|_| zbus::Error::Failure(format!("{destination} didn't answer")))?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(names: &[&str]) -> HashSet<String> {
        names.iter().map(|&n| n.to_owned()).collect()
    }

    #[test]
    fn pauses_only_what_plays_nowhere_else() {
        let tv = vec!["orpheus.send.TV".to_owned()];
        let kitchen = vec!["orpheus.send.KITCHEN".to_owned()];
        let speakers = vec!["alsa_output.speakers".to_owned()];
        let lost = set(&["orpheus.send.TV"]);
        let unavailable = set(&["orpheus.send.TV", "orpheus.send.KITCHEN"]);
        let streams = [
            // Plays only into the lost output.
            (Some(10), tv.as_slice()),
            // Also plays into one that is still there.
            (Some(20), tv.as_slice()),
            (Some(20), speakers.as_slice()),
            // Into the lost one and one lost before.
            (Some(30), tv.as_slice()),
            (Some(30), kitchen.as_slice()),
            // Plays elsewhere only.
            (Some(40), speakers.as_slice()),
            // Not known.
            (None, tv.as_slice()),
        ];
        let chosen: Vec<u32> = choose(&streams, &lost, &unavailable).into_iter().collect();
        assert_eq!(chosen, [10, 30]);
    }

    #[test]
    fn finds_the_player_of_a_helper_process() {
        let me = std::process::id();
        assert_eq!(owner(me, &HashSet::from([me])), Some(me));
        assert_eq!(owner(me, &HashSet::new()), None);
        // The test's parent runs another program (cargo).
        let parent = parent(me).unwrap();
        assert_eq!(owner(me, &HashSet::from([parent])), None);
    }
}
