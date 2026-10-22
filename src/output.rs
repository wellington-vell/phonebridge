//! Where decoded PCM goes: a PipeWire virtual microphone or stdout.

use std::io::{self, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::thread;
use std::time::Duration;

use crate::opus::{CHANNELS, RATE};

const SINK: &str = "wemic_sink";
const SOURCE: &str = "wemic";

pub enum Output {
    Pipe(ChildStdin),
    Stdout(io::Stdout),
}

impl Output {
    pub fn write(&mut self, pcm: &[u8]) -> io::Result<()> {
        match self {
            Output::Pipe(w) => w.write_all(pcm),
            Output::Stdout(w) => w.write_all(pcm),
        }
    }
}

fn spawn_err(prog: &str, e: io::Error) -> io::Error {
    io::Error::new(e.kind(), format!("{prog}: {e}"))
}

/// PipeWire virtual microphone: pw-loopback (sink -> source) fed by pw-cat.
/// Returns the writer and the child processes to reap on shutdown.
pub fn virtual_mic(name: &str) -> io::Result<(Output, Vec<Child>)> {
    let props = |class: &str, node: &str, desc: &str| {
        format!(
            "media.class={class} node.name={node} node.description=\"{desc}\" \
             audio.channels=1 audio.position=[MONO]"
        )
    };
    let lo = Command::new("pw-loopback")
        .arg("--capture-props")
        .arg(props("Audio/Sink", SINK, &format!("{name} (input)")))
        .arg("--playback-props")
        .arg(props("Audio/Source", SOURCE, name))
        .spawn()
        .map_err(|e| spawn_err("pw-loopback", e))?;
    thread::sleep(Duration::from_millis(500));

    let mut cat = Command::new("pw-cat")
        .args(["--playback", "--raw", "--target", SINK])
        .args([
            "--rate",
            &RATE.to_string(),
            "--channels",
            &CHANNELS.to_string(),
        ])
        .args(["--format", "s16", "--latency", "40ms", "-"])
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| spawn_err("pw-cat", e))?;
    let stdin = cat.stdin.take().expect("piped stdin");
    // pw-cat first so the loopback outlives its feeder.
    Ok((Output::Pipe(stdin), vec![cat, lo]))
}
